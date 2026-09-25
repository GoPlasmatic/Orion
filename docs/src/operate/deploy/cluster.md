<!-- description: Run Orion as a multi-node cluster: what replicas share, epoch-based engine reload, job leases, and the shared Redis that backs rate limits and dedup stores. -->
<!-- type: guide -->
<!-- last_verified: 2026-09-14 -->

# Run a cluster

*Cluster mode* makes N identical Orion replicas behave as one logical system: a change made through any node reaches all of them. The cross-request state that guards depend on, such as deduplication windows, rate-limit windows and response caches, is shared instead of per node. You do not need this page to run Orion on a single node.

## Before you start

Cluster mode needs two shared backends, and refuses to start without them:

- **PostgreSQL or MySQL.** Startup refuses `sqlite:`, because a file is single-host by construction.
- **A shared Redis.** This is where dedup, response caches and per-channel rate limits live.

| Backend | Single instance | Cluster mode | Notes |
|---------|:-:|:-:|-------|
| **SQLite** | Recommended | Refused at startup | WAL mode allows concurrent reads but one writer, and the file cannot be shared across hosts |
| **PostgreSQL** | Supported | Recommended | Use connection pooling (PgBouncer) when replica count × `storage.max_connections` approaches the server's limit |
| **MySQL** | Supported | Supported | Use `READ-COMMITTED` isolation for best concurrency |

## When to go cluster

Reach for cluster mode when you need one of these, in roughly this order:

- **Availability.** One node is one restart away from an outage. Two behind a load balancer survive a rolling deploy, a node failure and a kernel upgrade.
- **Correct guards across replicas.** The moment a second node exists, per-node dedup and rate limits stop meaning what they say. Cluster mode is what makes "100 requests per second" a fleet-wide number rather than a per-node one.
- **Config changes that fan out.** Without it, an activation reaches only the node that received the API call.

Throughput is usually the weakest reason. In the published Orion 1.0.0 test, a single instance sustained 5.1K to 5.7K workflow requests per second with single-digit-millisecond average latency. The [benchmark record](https://github.com/GoPlasmatic/Orion/blob/main/crates/orion-server/tests/benchmark/results/v1.0.0/SUMMARY.md) ran on an Apple M2 Pro Mac Mini with a release build and 50 concurrent connections. Treat that as a versioned benchmark result, not a capacity promise for another release, machine or workflow. Most estates hit an availability requirement long before they hit a throughput ceiling.

> [!NOTE]
> The published record measures a single instance. The repository ships a cluster load scenario (`crates/orion-server/tests/benchmark/bench.sh cluster`), but its numbers are not part of the 1.0.0 record. Size a fleet against your own measurements rather than assuming replicas multiply the figure above.

## Turn it on

Point every replica at the shared backends:

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

`instance_id` doubles as the Kafka `group.instance.id`. Give each replica a stable one if you want rolling restarts to rejoin without a full consumer-group rebalance.

## What the cluster shares

| Concern | How it works |
|---|---|
| **Config changes** | Every admin mutation advances a shared config epoch in the database, recording *what* it changed. Each replica polls it every `epoch_poll_interval_ms` and resyncs the parts that moved. See [How a change reaches every node](#how-a-change-reaches-every-node). |
| **Deduplication** | On the shared Redis: the same idempotency key on two nodes gets exactly one execution and a `409` for the replay. |
| **Response caching** | Shared, so a second node serves a warm cache instead of warming its own. |
| **Per-channel rate limits** | A shared fixed window; the configured rate holds across all replicas combined. |
| **Background jobs** | Trace cleanup and DLQ retry take a per-tick lease, so one node runs each job. DLQ rows are row-leased too, so each entry is retried once. |
| **Kafka consumers** | Static group membership keyed by `instance_id`; rolling restarts rejoin without a full rebalance. |
| **Circuit-breaker resets** | `POST /circuit-breakers/{key}` fans out over the epoch bus; one call resets the key everywhere. |

### How a change reaches every node

The bump carries a *scope*, and the replicas resync to it:

| What you changed | What every other node does |
|---|---|
| A workflow or a channel | Republishes its runtime generation (engine plus channel estate, one store). Connector pools are untouched. |
| A connector (create, update, delete, reload) | The above, plus reloads its connector registry and drops its cached SQL, MongoDB and cache pools, because the endpoint or the credentials behind a live connection may now be wrong. |

Only the second row costs reconnections, and that is the point. Before the scope existed the epoch was a bare counter, so every node answered every bump with the widest resync there is. One workflow activation dropped every pooled connection across the whole fleet.

A node running an older release bumps the epoch without writing a scope, and its peers read that as "everything". A mixed-version fleet therefore behaves as it did before: the reconnect storm, never a missed change. It stops as soon as every node is writing scopes.

That holds because the scope is stamped with the epoch it was written for, not only stored. The scope column is sticky. An older node's bump advances the counter and leaves whatever the last scope-aware node wrote still sitting there. Read at face value, a connector change made by an old node would arrive at its peers wearing the previous change's `definitions` label. They would skip the connector reload and pool eviction it needed, and serve the old endpoint and the old credentials until something else bumped. A scope counts only when its stamp matches the current epoch. Anything else is the widest resync, which is what an unattributable scope has always meant.

The row keeps one scope, so it describes one bump. A node that finds the epoch several ahead of what it last applied is applying all of those bumps in one resync. The scopes of the earlier ones were overwritten by the later ones. It resyncs wide, exactly as it would for a scope it could not attribute. That is the common case whenever changes come in a burst. Creating a connector and activating the workflow that uses it is three bumps, and they land well inside one `epoch_poll_interval_ms`. Peers pay one wide resync for the burst rather than one per change. The narrow scope does its work where it matters: a fleet at rest, where each change arrives on its own.

### A node that has recently started

A booting node reads the epoch before it loads channels and workflows. Anything committed while it was starting is picked up by its first poll rather than missed. It is at most one `epoch_poll_interval_ms` behind, exactly like every other node. What is worth knowing is that it is behind while serving. `/healthz` and `/readyz` are green as soon as the first generation is published, so a load balancer sends it traffic at once. A channel activated during that window answers `404` there and `200` on its peers until the tick lands.

A node with [`[packages] apply`](../../reference/configuration/packages.md) is the exception: its `/readyz` holds until its packages serve. Every replica may list the same artifact. One applies it while the others wait on its receipt, then reload and check.

That is normal eventual consistency, not a fault, and it is invisible at rest. It shows up when a deploy and a configuration change overlap. Roll a node and activate a channel in the same couple of seconds. A few requests meet the node that has not caught up yet. If that matters for a particular rollout, let the restarted node settle for one poll interval before making the change. Or make the change first and roll afterwards.

### When a change does not propagate

The bump happens after the mutation is committed and live on the node that served it. If the bump itself fails, because the database went away between the two, that node keeps serving the change. The others never hear about it. The request still succeeds, because it did: the row is written, and a `500` would only invite a retry that writes a second version.

The signal is on the node instead. `/health` carries a `config_propagation` component in cluster mode:

```json
{ "components": { "config_propagation": "degraded" } }
```

`degraded` means at least one bump has failed since the last successful one, and peers may be serving stale configuration. It clears on the next successful bump; any mutation will do, because a resync re-reads everything from the database rather than applying a delta. `/readyz` is deliberately unaffected. This node is correct, and taking it out of rotation would not tell the others. Alert on the component, and on `orion_errors_total{reason="config_epoch_bump"}`.

### Scheduled runs

Cron channels coordinate entirely through three shared tables and need no leader. An occurrence's identity is `(channel_id, scheduled_for)`, so two reconcilers racing the same pass produce one row. Claims are leased against the database clock, and a running attempt renews its claim every heartbeat. When a node dies, a peer recovers its work and its singleton slots after one `cron.claim_lease_secs`. A `forbid` singleton is a row exactly one occurrence holds at a time, acquired in the same transaction that marks it running.

Every node should agree on `cron.enabled`. A node with it off quarantines the active cron channels rather than ignoring them. A mixed cluster is visible on `/health` rather than silently half-scheduling.

## What stays per node

These are per node by design, and each has ×N semantics you should size for:

| Component | Semantics |
|---|---|
| **Circuit breakers** | Trip independently; each node stops calling after its own failures. Resets fan out. |
| **Backpressure** | `max_concurrent_per_node` is per node, as the name says: N replicas admit up to N× that many in flight. |
| **Platform rate limits** | `[rate_limit]` IP limits are per node: N× the configured value fleet-wide. |
| **`/metrics`** | Scraped per node. Point Prometheus at every replica, or let it discover pods. |

> [!WARNING]
> A channel whose dedup or cache connector is missing, broken, or explicitly in-memory refuses to load in cluster mode. The activating admin call succeeds; the channel is then quarantined at load, refused at every ingress with a `503`, absent from the route table, and listed under `/health`'s `channels.quarantined` with `components.channels: "degraded"`. The node boots and every other channel keeps serving. Silently degrading to per-node state would leave a channel advertising a guarantee it no longer keeps.

## Migrate as a deploy step, not at boot

Run `orion-server migrate` before new replicas start:

```toml
[storage]
auto_migrate = false
```

A replica that boots against a pending migration fails fast. A production cluster left on `auto_migrate = true` is refused at startup rather than allowed to race. Write migrations expand and contract style. First ship one that only *adds* columns, tables and indexes, beside code that works with both shapes; remove the old shape in a later release. During a rolling deploy, old and new binaries briefly share one database.

Both packaged deployments wire this in already. The Helm chart runs migrations as a `pre-install` and `pre-upgrade` Job, and `docker-compose.ha.yml` has a one-shot `migrate` service that completes before either node boots.

## Verify

Confirm that a change made through one node reaches the others:

```bash
curl -s -X POST http://node-a:8080/api/v1/admin/engine/reload
sleep 3
curl -s http://node-b:8080/health | jq '{status, config_propagation: .components.config_propagation}'
```

`config_propagation` reads `ok` on every node, and an activation made through node A answers on node B within one `epoch_poll_interval_ms`. `deploy/ha/rolling-drill.sh` drives a zero-5xx rolling deploy against the compose topology. `deploy/ha/plugin-drill.sh` activates a [plugin](../../concepts/plugins.md) on one node and waits for it to converge on the other.

## Backups change shape in a cluster

`POST /api/v1/admin/backups` returns `400` in cluster mode. The file would land on one arbitrary replica, and cluster storage is PostgreSQL or MySQL, which the SQLite backup mechanism cannot copy anyway. Use your database's own tooling: automated snapshots plus point-in-time recovery on a managed service, or `pg_dump` and `mysqldump` self-managed. Redis needs no backup; everything in it is reconstructible ephemeral state. See [Back up and restore](../maintain/backup-restore.md).

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
