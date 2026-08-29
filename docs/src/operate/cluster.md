<!-- description: Run Orion as a multi-node cluster: what replicas share, epoch-based engine reload, job leases, and the shared Redis that backs rate limits and dedup stores. -->
# Cluster Mode & High Availability

**Cluster mode** makes N identical Orion replicas behave as one logical system:
a change made through any node reaches all of them, and the cross-request state
that guards depend on — deduplication windows, rate-limit windows, response
caches — is shared instead of per node.

You do not need this page to run Orion on a single node.

## When to go cluster

Reach for cluster mode when you need one of these, in roughly this order:

- **Availability.** One node is one restart away from an outage. Two behind a
  load balancer survive a rolling deploy, a node failure, and a kernel upgrade.
- **Correct guards across replicas.** The moment a second node exists, per-node
  dedup and rate limits stop meaning what they say. Cluster mode is what makes
  "100 requests per second" a fleet-wide number rather than a per-node one.
- **Config changes that fan out.** Without it, an activation reaches only the
  node that received the API call.

Throughput is usually the *weakest* reason. A single instance sustains
**5.1K–5.7K workflow requests/sec** with single-digit-millisecond average
latency in the
[v1.0.0 benchmark record](https://github.com/GoPlasmatic/Orion/blob/main/crates/orion-server/tests/benchmark/results/v1.0.0/SUMMARY.md)
(Apple M2 Pro Mac Mini, release build, 50 concurrent connections). Most estates
hit an availability requirement long before they hit that ceiling.

> [!NOTE]
> The published record measures a **single instance**. The repository ships a
> cluster load scenario
> (`crates/orion-server/tests/benchmark/bench.sh cluster`), but its numbers are
> not part of the v1.0.0 record. Size a fleet against your own measurements
> rather than assuming replicas multiply the figure above.

## Requirements

Cluster mode needs two shared backends, and refuses to start without them:

- **PostgreSQL or MySQL.** Startup refuses `sqlite:` — a file is single-host by
  construction.
- **A shared Redis.** This is where dedup, response caches, and per-channel rate
  limits live.

```toml
[cluster]
enabled = true
redis_url = "redis://redis:6379"   # required
epoch_poll_interval_ms = 2000      # how often nodes poll for config changes
instance_id = ""                   # auto-generated UUID when empty; max 64 chars

[storage]
url = "postgres://orion:orion@postgres:5432/orion"
auto_migrate = false               # run `orion-server migrate` as a deploy step
```

`instance_id` doubles as the Kafka `group.instance.id`, so give each replica a
stable one if you want rolling restarts to rejoin without a full consumer-group
rebalance.

| Backend | Single instance | Cluster mode | Notes |
|---------|:-:|:-:|-------|
| **SQLite** | Recommended | Refused at startup | WAL mode allows concurrent reads but one writer, and the file cannot be shared across hosts |
| **PostgreSQL** | Supported | Recommended | Use connection pooling (PgBouncer) when replica count × `storage.max_connections` approaches the server's limit |
| **MySQL** | Supported | Supported | Use `READ-COMMITTED` isolation for best concurrency |

## What the cluster shares

| Concern | How it works |
|---|---|
| **Config changes** | Every admin mutation advances a shared config epoch in the database, recording *what* it changed. Each replica polls it every `epoch_poll_interval_ms` and resyncs the parts that moved. A change through *any* node reaches *all* nodes — see [How a change reaches every node](#how-a-change-reaches-every-node). |
| **Deduplication** | On the shared Redis: the same idempotency key on two nodes gets exactly one execution and a `409` for the replay. |
| **Response caching** | Shared, so a second node serves a warm cache instead of warming its own. |
| **Per-channel rate limits** | A shared fixed window — the configured rate holds across all replicas combined. |
| **Background jobs** | Trace cleanup and DLQ retry take a per-tick lease, so one node runs each job. DLQ rows are additionally row-leased, so each entry is retried once. |
| **Kafka consumers** | Static group membership keyed by `instance_id`; rolling restarts rejoin without a full rebalance. |
| **Circuit-breaker resets** | `POST /circuit-breakers/{key}` fans out over the epoch bus — one call resets the key everywhere. |

### How a change reaches every node

The bump carries a **scope**, and the replicas resync to it:

| What you changed | What every other node does |
|---|---|
| A workflow or a channel | Rebuilds its engine and channel registry. Connector pools are untouched. |
| A connector (create, update, delete, reload) | The above, plus reloads its connector registry and drops its cached SQL, MongoDB and cache pools — the endpoint or the credentials behind a live connection may now be wrong. |

Only the second row costs reconnections, and that is the point. Before the
scope existed the epoch was a bare counter, so every node answered every bump
with the widest resync there is: one workflow activation dropped every pooled
connection across the whole fleet.

A node running an older release bumps the epoch without writing a scope, and
its peers read that as "everything". A mixed-version fleet therefore behaves as
it did before — the reconnect storm, never a missed change — and stops as soon
as every node is writing scopes.

That holds because the scope is *stamped with the epoch it was written for*,
not merely stored. The scope column is sticky: an older node's bump advances
the counter and leaves whatever the last scope-aware node wrote still sitting
there. Read at face value, a connector change made by an old node would arrive
at its peers wearing the previous change's `definitions` label, and they would
skip the connector reload and pool eviction it needed — serving the old
endpoint and the old credentials until something else bumped. A scope counts
only when its stamp matches the current epoch; anything else is the widest
resync, which is what an unattributable scope has always meant.

### When a change does not propagate

The bump happens after the mutation is committed and live on the node that
served it. If the bump itself fails — the database went away between the two —
that node keeps serving the change and the others never hear about it. The
request still succeeds, because it did: the row is written, and a `500` would
only invite a retry that writes a second version.

The signal is on the node instead. `/health` carries a `config_propagation`
component in cluster mode:

```json
{ "components": { "config_propagation": "degraded" } }
```

`degraded` means at least one bump has failed since the last successful one,
and peers may be serving stale configuration. It clears on the next successful
bump — any mutation will do, because a resync re-reads everything from the
database rather than applying a delta. `/readyz` is deliberately unaffected:
this node is correct, and taking it out of rotation would not tell the others.
Alert on the component, and on `orion_errors_total{reason="config_epoch_bump"}`.

## What stays per node

These are per-node **by design**, and each has ×N semantics you should size for:

| Component | Semantics |
|---|---|
| **Circuit breakers** | Trip independently — each node stops calling after its own failures. Resets fan out (above). |
| **Backpressure** | `max_concurrent_per_node` is per node, as the name says: N replicas admit up to N× that many in flight. |
| **Platform rate limits** | `[rate_limit]` IP limits are per node — N× the configured value fleet-wide. |
| **`/metrics`** | Scraped per node. Point Prometheus at every replica, or let it discover pods. |

> [!WARNING]
> **A channel whose dedup or cache connector is missing, broken, or explicitly
> in-memory refuses to load in cluster mode.** The activating admin call
> succeeds; the channel is then quarantined at load — refused at every ingress
> with a `503`, absent from the route table, logged as `Channel quarantined`,
> and listed under `/health`'s `channels.quarantined` with
> `components.channels: "degraded"` — while the node boots and every other
> channel keeps serving. Silently degrading to per-node state would leave a
> channel advertising a guarantee it no longer keeps.

## Migrate as a deploy step, not at boot

```toml
[storage]
auto_migrate = false
```

Run `orion-server migrate` before new replicas start. A replica that boots
against a pending migration fails fast, and a production cluster left on
`auto_migrate = true` is **refused at startup** rather than allowed to race.

Write migrations expand/contract style: first ship one that only *adds* —
columns, tables, indexes — alongside code that works with both shapes, and
remove the old shape in a later release. During a rolling deploy, old and new
binaries briefly share one database.

Both packaged deployments wire this in already: the Helm chart runs migrations
as a `pre-install`/`pre-upgrade` Job, and `docker-compose.ha.yml` has a one-shot
`migrate` service that completes before either node boots.

## Two packaged topologies

- **[Kubernetes (Helm)](./kubernetes.md)** — `deploy/helm/orion` deploys the
  cluster shape: 2 replicas by default, the pre-upgrade migration Job, surge
  rolling deploys, a PodDisruptionBudget, anti-affinity, hardened pod defaults,
  and a dedicated metrics listener. It installs as
  `ORION_ENVIRONMENT=production`, so admin keys are required up front.
- **`docker-compose.ha.yml`** (repository root) — the reference topology for one
  host or a smoke test of the production shape: nginx → 2× Orion in cluster mode
  → shared Postgres + Redis, plus the one-shot `migrate` service.
  `deploy/ha/rolling-drill.sh` drives a zero-5xx rolling deploy against it.

## Backups change shape in a cluster

`POST /api/v1/admin/backups` returns `400` in cluster mode. The file would land
on one arbitrary replica, and cluster storage is PostgreSQL or MySQL, which the
SQLite backup mechanism cannot copy anyway.

Use your database's own tooling: automated snapshots plus point-in-time recovery
on a managed service, or `pg_dump` / `mysqldump` self-managed. Redis needs no
backup — everything in it is reconstructible ephemeral state. See
[Back Up & Restore](./backup-restore.md).

## Sharding channels across pools

Cluster mode scales one estate. If you want *dedicated capacity* for a group of
channels, run separate instance pools against the same database and filter what
each loads:

```toml
# Pool A: order processing
[channel_filter]
include = ["orders.*", "payments.*"]

# Pool B: analytics
[channel_filter]
include = ["analytics.*", "reports.*"]
```

This is a refinement layered on top of cluster mode, not an alternative to it —
each pool is still a cluster if it has more than one node.

## Related

- [Deploy on Kubernetes (Helm)](./kubernetes.md) — the chart that implements
  this shape.
- [Deploy with Docker](./docker.md) — the compose topology, and the single-node
  case.
- [Timeouts, Retries & Circuit Breakers](./failure-handling.md) — the drain
  sequence a rolling deploy depends on.
- [Monitoring & Alerts](./monitoring.md) — scraping a fleet, and the per-node
  metrics that need aggregating.
- [Configuration Reference](../reference/configuration.md#cluster-ha) — every
  `[cluster]` key with its default.
