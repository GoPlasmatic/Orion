<!-- description: The [cluster] settings: enabling multi-replica mode, the shared Redis, the epoch poll interval and the per-replica instance_id, with their defaults. -->
<!-- type: reference -->
<!-- last_verified: 2026-09-14 -->

# Cluster settings

The `[cluster]` section: turning on multi-replica mode, the shared Redis, epoch polling and the per-replica identity.

## Synopsis

```toml
[cluster]
enabled = false
redis_url = ""
epoch_poll_interval_ms = 2000
instance_id = ""
```

## Description

With `cluster.enabled = false` — the default — Orion is a plain single node: no epoch watcher, no shared backends, no job leases. Enable it and N replicas sharing one PostgreSQL/MySQL and one Redis behave as a single logical system:

- **Config changes propagate.** A workflow or channel edited through any node bumps a database epoch; every other node notices within `epoch_poll_interval_ms` and reloads. Without this, an edit only affects the replica that received it.
- **Dedup and response caches default to the shared Redis**, so idempotency and caching are fleet-wide rather than per-node. A channel whose dedup store would silently degrade to node-local memory refuses to load instead.
- **Rate-limit windows are shared**, so a channel's limit is the fleet's limit and not N times it.
- **Background jobs single-flight.** Trace cleanup, audit cleanup, and DLQ retry run on one node at a time behind a lease.

Cluster mode requires `postgres://` or `mysql://` storage — SQLite is single-host by construction and is rejected at startup. `instance_id` is capped at 64 characters because it doubles as the Kafka `group.instance.id`.

```toml
[storage]
url = "postgres://orion:secret@postgres:5432/orion"
auto_migrate = false

[cluster]
enabled = true
redis_url = "redis://redis:6379"
instance_id = "${HOSTNAME}"
```

## Options

| Setting | Default | Env var | When to change |
|---|---|---|---|
| `cluster.enabled` | `false` | `ORION_CLUSTER__ENABLED` | Turn on whenever more than one Orion process serves the same database. |
| `cluster.redis_url` | `""` | `ORION_CLUSTER__REDIS_URL` | Required when enabled, for example `redis://cache:6379`. |
| `cluster.epoch_poll_interval_ms` | `2000` | `ORION_CLUSTER__EPOCH_POLL_INTERVAL_MS` | Lower for faster config propagation, at the cost of more database polling. |
| `cluster.instance_id` | `""` | `ORION_CLUSTER__INSTANCE_ID` | Set a stable per-replica value (for example the pod name) so Kafka static membership survives restarts. Empty generates a UUID per boot. |

## Related

- [Deploy a cluster](../../operate/deploy/cluster.md): the topology, and what shared state a cluster keeps.
- [Deploy with Kubernetes](../../operate/deploy/kubernetes.md): the chart that sets these.
- [Architectural characteristics](../../concepts/architectural-characteristics.md): what cluster mode guarantees.
- [Server configuration](./index.md): every section, by what you are configuring.
