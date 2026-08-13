# Production Checklist

Everything below is off or permissive by default, because the defaults serve a
laptop. Work through this before an instance takes traffic you did not send it
yourself.

Setting `environment = "production"` makes exactly five things fatal at startup
rather than advisory: `admin_auth` disabled, an admin key too weak to be one, a
`[cors] allowed_origins = ["*"]` wildcard, `server.verbose_errors = true`, and
`cluster.enabled` together with `storage.auto_migrate`. Every other row below —
**TLS and per-channel data-plane `auth` included** — is never checked for you.

## Before it takes traffic

| Area | Do this | Owner |
|---|---|---|
| **Environment** | `ORION_ENVIRONMENT=production` — makes missing admin auth and wildcard CORS startup errors instead of warnings. | [Configuration](../reference/configuration.md#deployment-environment) |
| **Admin auth** | `admin_auth.enabled = true` with at least one strong key, ideally `sha256:<digest>`. Add a second key so rotation needs no downtime. | [Secure an Instance](./security.md#authenticate-the-admin-plane) |
| **Data-plane auth** | Decide per channel: an `auth` block, or a proxy in front. The data plane is open by default. | [Secure an Instance](./security.md#decide-how-the-data-plane-authenticates) |
| **TLS** | Terminate it — `server.tls` here, or at a load balancer in front. | [Secure an Instance](./security.md#terminate-tls) |
| **CORS** | Replace `["*"]` with explicit origins. | [Channel Configuration](../reference/channel-config.md#cors--origins) |
| **Trusted proxies** | `rate_limit.trusted_proxies` if anything proxies to Orion — otherwise every caller shares one rate-limit bucket. | [Secure an Instance](./security.md#trust-the-right-proxies) |
| **Secrets** | Every connector authored with `env://` or `vault://`, never a literal. Set `storage.connector_encryption_key`. | [Secure an Instance](./security.md#keep-credentials-out-of-the-database) |
| **API docs** | `server.docs.enabled = false` in production, so the admin surface is not published to anonymous callers. | [OpenAPI](../reference/openapi.md) |
| **Database** | PostgreSQL or MySQL for anything multi-replica. Size `storage.max_connections` against the server's limit ÷ replica count. | [Cluster Mode](./cluster.md#requirements) |
| **Cluster** | More than one replica? `cluster.enabled = true` with a shared `redis_url`, `auto_migrate = false`, and `orion-server migrate` as a deploy step. Without it, a config change reaches only the node that received it. | [Cluster Mode](./cluster.md) |
| **Rate limiting** | `rate_limit.enabled = true`, sized per channel. | [Channel Configuration](../reference/channel-config.md#rate-limiting) |
| **Circuit breakers** | `engine.circuit_breaker.enabled = true` when workflows call external services. Off by default. | [Failure Handling](./failure-handling.md#stop-calling-a-failing-backend) |
| **Retention** | Bound `trace_queue.retention_hours` and `audit.retention_days`. Nothing else trims those tables. | [Traces](./traces.md#keep-the-table-bounded) · [Audit Logs](./audit-logs.md#bound-retention) |
| **Observability** | `metrics.enabled = true` with a dedicated `bind_addr`, `logging.format = "json"`, `tracing.enabled = true` pointed at a collector. | [Monitoring](./monitoring.md) |
| **Alerts** | The five silent signals, not just error rate and latency. | [Monitoring › What to alert on](./monitoring.md#what-to-alert-on) |
| **Kafka** | Managed broker? `[kafka.auth]` with `sasl_ssl`, and `kafka.dlq.enabled = true` so a poison message cannot stall a partition. | [Configuration](../reference/configuration.md#kafka) |
| **Shutdown** | Keep `shutdown_drain_secs + shutdown_force_timeout_secs` under your orchestrator's termination grace period. | [Failure Handling](./failure-handling.md#shut-down-without-dropping-requests) |
| **Backups** | A backup that leaves the host, and a restore you have actually run once. | [Back Up & Restore](./backup-restore.md) |

## Verify it, do not assume it

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

`"status": "degraded"` at HTTP 200 is the case worth checking for by hand — it
means a connector failed to load or a channel is quarantined while the instance
keeps serving everything else.

## Before each deploy

- **Read the version's upgrade notes** and run `preflight` with the new binary
  against the current database. See [Upgrades](./upgrades.md).
- **Back up first.** Every other step is reversible once this one happened.
- **Migrate as a deploy step** in a cluster, not at boot.
- **Roll one node at a time**, and confirm `/readyz` on each before moving on.

## Related

- [Secure an Instance](./security.md) — the security rows, in detail.
- [Cluster Mode & High Availability](./cluster.md) — the multi-replica rows.
- [Monitoring & Alerts](./monitoring.md) — what to watch once this is live.
- [Troubleshooting](./troubleshooting.md) — when one of these turns out to have
  been missed.
