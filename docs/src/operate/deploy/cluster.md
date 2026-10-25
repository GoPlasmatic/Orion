<!-- description: Run Orion as a multi-node cluster: what replicas share, how a deployment reaches every node, job leases, and the database behind rate limits and caches. -->
<!-- type: guide -->
<!-- last_verified: 2026-10-25 -->

# Run a cluster

*Cluster mode* makes N identical Orion replicas behave as one logical system: a deployment applied through any node reaches all of them. The cross-request state that guards depend on, such as deduplication windows, rate-limit windows and response caches, is shared instead of per node. You do not need this page to run Orion on a single node.

## Before you start

Cluster mode needs one shared backend: **PostgreSQL or MySQL**. Startup refuses `sqlite:`, because a file is single-host by construction.

That database is all the replicas share. The deployment history, job leases and the cron ledger live in it, and so do dedup claims, response caches and per-channel rate-limit windows. A cluster needs no Redis or other coordination service.

| Backend | Single instance | Cluster mode | Notes |
|---------|:-:|:-:|-------|
| **SQLite** | Recommended | Refused at startup | WAL mode allows concurrent reads but one writer, and the file cannot be shared across hosts |
| **PostgreSQL** | Supported | Recommended | Use connection pooling (PgBouncer) when replica count × `storage.max_connections` approaches the server's limit |
| **MySQL** | Supported | Supported | Use `READ-COMMITTED` isolation for best concurrency |

## When to go cluster

Reach for cluster mode when you need one of these, in roughly this order:

- **Availability.** One node is one restart away from an outage. Two behind a load balancer survive a rolling deploy, a node failure and a kernel upgrade.
- **Correct guards across replicas.** The moment a second node exists, per-node dedup and rate limits stop meaning what they say. Cluster mode is what makes "100 requests per second" a fleet-wide number rather than a per-node one.
- **Deployments that every node serves.** Every replica follows the same deployment head, and convergence reports which nodes serve it.

Throughput is usually the weakest reason. In the published Orion 1.0.0 test, a single instance sustained 5.1K to 5.7K workflow requests per second with single-digit-millisecond average latency. The [benchmark record](https://github.com/GoPlasmatic/Orion/blob/main/crates/orion-server/tests/benchmark/results/v1.0.0/SUMMARY.md) ran on an Apple M2 Pro Mac Mini with a release build and 50 concurrent connections. Treat that as a versioned benchmark result, not a capacity promise for another release, machine or workflow. Most estates hit an availability requirement long before they hit a throughput ceiling.

> [!NOTE]
> The published record measures a single instance. The repository ships a cluster load scenario (`crates/orion-server/tests/benchmark/bench.sh cluster`), but its numbers are not part of the 1.0.0 record. Size a fleet against your own measurements rather than assuming replicas multiply the figure above.

## Turn it on

Point every replica at the shared database:

```toml
[cluster]
enabled = true
epoch_poll_interval_ms = 2000      # how often each node's reconciler polls the database
instance_id = ""                   # auto-generated UUID when empty; max 64 chars

[storage]
url = "postgres://orion:orion@postgres:5432/orion"
auto_migrate = false               # run `orion-server migrate` as a deploy step
```

`instance_id` names this node as a lease holder and in log lines. Give each replica a stable one if you want those to survive a rolling restart.

## What the cluster shares

| Concern | How it works |
|---|---|
| **Deployments** | A committed deployment is a row in the database. Each replica's reconciler polls the head every `epoch_poll_interval_ms` and builds the revision it should serve. See [How a deployment reaches every node](#how-a-deployment-reaches-every-node). |
| **Deduplication** | In the shared database: the same idempotency key on two nodes gets exactly one execution and a `409` for the replay. Expiry is judged on the database's clock, so nodes with skewed clocks agree. |
| **Response caching** | In the shared database, so a second node serves a warm cache instead of warming its own. |
| **Per-channel rate limits** | A one-second fixed window counted in the shared database; the configured rate holds across all replicas combined. Each limited request costs a short write transaction, so a busy rate-limited channel is database load to size for. |
| **Background jobs** | Trace cleanup and DLQ retry take a per-tick lease, so one node runs each job. DLQ rows are row-leased too, so each entry is retried once. |
| **Circuit-breaker resets** | `POST /circuit-breakers/{key}` is recorded in the database, and every node's reconciler applies it. One call resets the key everywhere. |

Every node also runs a sweeper that deletes expired dedup, cache and rate-limit rows from the shared tables every 30 seconds. Concurrent sweeps are harmless. An expired row already reads as absent, so a sweeper that stops leaves every answer correct and only lets the tables grow.

### How a deployment reaches every node

An apply commits a new head to the shared database and builds it on the node that took the request. Nothing pushes it to the other nodes. Each one finds it through its own reconcile loop, the one loop that changes what a node serves.

Each tick, the loop reads the committed head. When the head has moved, it re-reads the deployment history and builds the revision it should serve. It also applies circuit-breaker resets requested on other nodes, and writes a heartbeat naming the revision it serves.

A build that fails leaves the node serving what it had, and its `/health` reports `engine_reload` as degraded. The loop retries on the next tick. `GET /api/v2/admin/deployments/{revision}/convergence?target=observed` reports which nodes serve a revision, from those heartbeats. [Deployments](../../concepts/deployments.md#convergence) explains the model.

### A node that has recently started

A booting node is at most one `epoch_poll_interval_ms` behind, like every other node. It is behind while serving. `/healthz` and `/readyz` are green once the first generation is published, so a load balancer sends it traffic at once. A deployment committed during that window can answer `404` there and `200` on its peers until the tick lands.

That is normal eventual consistency, not a fault. It shows up when a rolling restart and a deployment overlap. If that matters, let the restarted node settle for one poll interval before you apply. Or apply first and roll afterwards.

### Scheduled runs

Cron channels coordinate entirely through three shared tables and need no leader. An occurrence's identity is `(channel_id, scheduled_for)`, so two reconcilers racing the same pass produce one row. Claims are leased against the database clock, and a running attempt renews its claim every heartbeat. When a node dies, a peer recovers its work and its singleton slots after one `cron.claim_lease_secs`. A `forbid` singleton is a row exactly one occurrence holds at a time, acquired in the same transaction that marks it running.

Every node should agree on `cron.enabled`. A node with it off quarantines the deployed cron channels rather than ignoring them. A mixed cluster is visible on `/health` rather than silently half-scheduling.

## What stays per node

These are per node by design, and each has ×N semantics you should size for:

| Component | Semantics |
|---|---|
| **Circuit breakers** | Trip independently; each node stops calling after its own failures. Resets fan out. |
| **Backpressure** | `max_concurrent_per_node` is per node, as the name says: N replicas admit up to N× that many in flight. |
| **Platform rate limits** | `[rate_limit]` IP limits are per node: N× the configured value fleet-wide. |
| **`/metrics`** | Scraped per node. Point Prometheus at every replica, or let it discover pods. |

> [!WARNING]
> A channel whose dedup or cache connector is missing, broken, or explicitly in-memory refuses to load in cluster mode. The deployment commits; the channel is then quarantined at load, refused at every ingress with a `503`, absent from the route table, and listed under `/health`'s `channels.quarantined` with `components.channels: "degraded"`. The node boots and every other channel keeps serving. Silently degrading to per-node state would leave a channel advertising a guarantee it no longer keeps.

## Migrate as a deploy step, not at boot

Run `orion-server migrate` before new replicas start:

```toml
[storage]
auto_migrate = false
```

A replica that boots against a pending migration fails fast. A production cluster left on `auto_migrate = true` is refused at startup rather than allowed to race. Write migrations expand and contract style. First ship one that only *adds* columns, tables and indexes, beside code that works with both shapes; remove the old shape in a later release. During a rolling deploy, old and new binaries briefly share one database.

Both packaged deployments wire this in already. The Helm chart runs migrations as a `pre-install` and `pre-upgrade` Job, and `docker-compose.ha.yml` has a one-shot `migrate` service that completes before either node boots.

## Verify

Confirm that a deployment applied through one node reaches the others:

```bash
curl -s http://node-a:8080/api/v2/admin/deployments | jq '.head_revision'
curl -s "http://node-b:8080/api/v2/admin/deployments/1/convergence?target=observed"
```

Every node appears under `converged_nodes` within one `epoch_poll_interval_ms`. `deploy/ha/rolling-drill.sh` drives a zero-5xx rolling deploy against the compose topology.

## Backups change shape in a cluster

`POST /api/v2/admin/backups` returns `400` in cluster mode. The file would land on one arbitrary replica, and cluster storage is PostgreSQL or MySQL, which the SQLite backup mechanism cannot copy anyway. Use your database's own tooling: automated snapshots plus point-in-time recovery on a managed service, or `pg_dump` and `mysqldump` self-managed. The shared-state tables, `cluster_kv` and `cluster_rate_windows`, need no special care. Everything in them is short-lived dedup, cache and rate-limit state that expires on its own. See [Back up and restore](../maintain/backup-restore.md).

## Shard channels across pools

Cluster mode scales one estate. If you want dedicated capacity for a group of channels, run separate instance pools against the same database and filter what each loads:

```toml
# Pool A: order processing
[channel_filter]
include = ["orders.*", "payments.*"]

# Pool B: analytics
[channel_filter]
include = ["analytics.*", "reports.*"]
```

This is a refinement layered on top of cluster mode, not an alternative to it. Each pool is still a cluster if it has more than one node.

## Next steps

- [Deploy on Kubernetes](./kubernetes.md): the chart that implements this shape.
- [Deploy with Docker](./docker.md): the compose topology, and the single-node case.
- [Timeouts, retries and circuit breakers](../run/failure-handling.md): the drain sequence a rolling deploy depends on.
- [Monitor and alert](../run/monitoring.md): scraping a fleet, and the per-node metrics that need aggregating.
- [Configuration › Cluster](../../reference/configuration/cluster.md): every `[cluster]` key with its default.
