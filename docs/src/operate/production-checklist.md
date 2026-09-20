<!-- description: Everything in Orion is off or permissive by default because the defaults serve a laptop. Work through this before an instance takes traffic you did not send. -->
<!-- type: guide -->
<!-- last_verified: 2026-09-19 -->

# Production checklist

Everything below is off or permissive by default, because the defaults serve a laptop. Work through this before an instance takes traffic you did not send it yourself. Each row names the page that owns the procedure.

## Before you start

You need admin access to the instance and to its config, and `orion-server` on a machine that can reach its database.

Setting `environment = "production"` makes exactly five things fatal at startup rather than advisory. They are `admin_auth` disabled, an admin key too weak to be one, a `[cors] allowed_origins = ["*"]` wildcard, `server.verbose_errors = true`, and `cluster.enabled` together with `storage.auto_migrate`. Every other row below, TLS and per-channel data-plane `auth` included, is never checked for you.

## Before it takes traffic

| Area | Do this | Owner |
|---|---|---|
| **Environment** | `ORION_ENVIRONMENT=production`, so missing admin auth and wildcard CORS are startup errors instead of warnings. | [Deployment environment](../reference/configuration/environment.md) |
| **Admin auth** | `admin_auth.enabled = true` with at least one strong key, ideally `sha256:<digest>`. Add a second key so rotation needs no downtime. | [Authenticate the admin plane](./run/security.md#authenticate-the-admin-plane) |
| **Data-plane auth** | Decide per channel: an `auth` block, or a proxy in front. The data plane is open by default. | [Decide how the data plane authenticates](./run/security.md#decide-how-the-data-plane-authenticates) |
| **TLS** | Terminate it: `server.tls` here, or at a load balancer in front. | [Terminate TLS](./run/security.md#terminate-tls) |
| **CORS** | Replace `["*"]` with explicit origins. Declare any custom request header your browser client sends; `allow_credentials` requires explicit origins. | [Configuration › CORS](../reference/configuration/cors.md) |
| **Trusted proxies** | `rate_limit.trusted_proxies` if anything proxies to Orion; otherwise every caller shares one rate-limit bucket. | [Trust the right proxies](./run/security.md#trust-the-right-proxies) |
| **Secrets** | Every connector authored with `env://` or `vault://`, never a literal. Set `storage.connector_encryption_key`. | [Keep credentials out of the database](./run/security.md#keep-credentials-out-of-the-database) |
| **API docs** | `server.docs.enabled = false` in production, so the admin surface is not published to anonymous callers. | [OpenAPI specification](../reference/openapi.md) |
| **Database** | PostgreSQL or MySQL for anything multi-replica. Size `storage.max_connections` against the server's limit divided by the replica count. | [Requirements](./deploy/cluster.md#before-you-start) |
| **Cluster** | More than one replica? `cluster.enabled = true` with a shared `redis_url`, `auto_migrate = false`, and `orion-server migrate` as a deploy step. Without it, a config change reaches only the node that received it. | [Run a cluster](./deploy/cluster.md) |
| **Rate limiting** | `rate_limit.enabled = true`, sized per channel. | [Rate limiting](../reference/channel-config/rate_limit.md) |
| **Circuit breakers** | `engine.circuit_breaker.enabled = true` when workflows call external services. Off by default. | [Stop calling a failing backend](./run/failure-handling.md#stop-calling-a-failing-backend) |
| **Retention** | Bound `trace_queue.retention_hours` and `audit.retention_days`. Nothing else trims those tables. | [Keep the table bounded](./run/traces.md#keep-the-table-bounded) · [Bound retention](./run/audit-logs.md#bound-retention) |
| **Observability** | `metrics.enabled = true` with a dedicated `bind_addr`, `logging.format = "json"`, `tracing.enabled = true` pointed at a collector. | [Monitor and alert](./run/monitoring.md) |
| **Alerts** | The seven silent signals, not only error rate and latency. | [What to alert on](./run/monitoring.md#what-to-alert-on) |
| **Kafka** | Managed broker? `[kafka.auth]` with `sasl_ssl`, and `kafka.dlq.enabled = true` so a poison message cannot stall a partition. | [Configuration › Kafka](../reference/configuration/kafka.md) |
| **Plugins** | Leave `plugins.enabled = false` unless you run one. If you do, name signing keys in `[plugins.trust]` (`orion-server plugin keygen` creates one) and size the ceilings. The pooling allocator reserves `max_live_instances × max_memory_bytes` of virtual address space at startup, 16 GiB by default; count it where a container limits virtual memory. | [Bound what a plugin can do](./run/security.md#bound-what-a-plugin-can-do) · [Configuration › Plugins](../reference/configuration/plugins.md) |
| **Expression budget** | `engine.ops_budget` whenever expressions come from someone other than the operator: tenant rules, competitor-written model adapters. Size it from the heaviest legitimate expression; a condition that crosses it fails closed to `false`, silently. | [Configuration › Engine](../reference/configuration/engine.md) |
| **Shutdown** | Keep `shutdown_drain_secs + shutdown_force_timeout_secs` under your orchestrator's termination grace period. | [Shut down without dropping requests](./run/failure-handling.md#shut-down-without-dropping-requests) |
| **Backups** | A backup that leaves the host, and a restore you have run once. | [Back up and restore](./maintain/backup-restore.md) |

## Verify

Three commands answer most of the list above against a real instance:

```bash
orion-server validate-config -c config.toml   # config file + ORION_* environment
orion-server test-connectivity -c config.toml # the database, and Kafka when enabled
orion-server preflight -c config.toml         # stored channels and workflows
```

Then confirm the running instance agrees:

```bash
curl -s http://localhost:8080/health | jq '{status, workflows_loaded, channels, connectors}'
```

`"status": "degraded"` at HTTP 200 is the case worth checking for by hand. It means a connector failed to load or a channel is quarantined while the instance keeps serving everything else.

## Before each deploy

- **Read the version's upgrade notes** and run `preflight` with the new binary against the current database. See [Upgrade an instance](./maintain/upgrades.md).
- **Back up first.** Every other step is reversible once this one happened.
- **Migrate as a deploy step** in a cluster, not at boot.
- **Roll one node at a time**, and confirm `/readyz` on each before moving on.

## Next steps

- [Secure an instance](./run/security.md): the security rows, in detail.
- [Run a cluster](./deploy/cluster.md): the multi-replica rows.
- [Monitor and alert](./run/monitoring.md): what to watch once this is live.
- [Troubleshooting](./maintain/troubleshooting.md): when one of these turns out to have been missed.
