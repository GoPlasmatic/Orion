# Config Reference

Every setting Orion has, with its real default and its environment variable. All settings have sensible defaults — `orion-server` with no config file at all starts and works. What follows is what you change when you want something other than a single-node development instance.

Defaults on this page are checked against `src/config/*.rs` by an integration test, so they cannot drift from the code. A ready-to-edit file carrying the same values lives at [`config.toml.example`](https://github.com/GoPlasmatic/Orion/blob/main/crates/orion-server/config.toml.example) — it is also what the Docker image ships at `/app/config.toml`.

## CLI Commands

Every `orion-server` subcommand — and the whole `orion-cli` surface — is
documented in the [CLI Reference](./cli.md). All subcommands honor `${VAR}` /
`${VAR:-default}` substitution in the loaded config file, so the same
`config.toml` can be reused across environments.

## How Settings Are Resolved

Three layers, in increasing precedence:

1. **Struct defaults** — everything on this page.
2. **The config file**, passed with `-c`. Values may reference process environment variables with `${VAR}` (required — startup fails if unset) or `${VAR:-default}` (optional). `$$` escapes a literal `$`. The same substitution runs against connector `config_json` blobs at startup, so secrets can stay out of the database. The complementary `env://VAR_NAME` resolver runs **after** JSON parsing on connector string fields: `${VAR}` rewrites text, `env://` rewrites parsed values.
3. **Environment variables**, named `ORION_SECTION__KEY` with a double underscore between levels — `ORION_SERVER__PORT`, `ORION_ENGINE__CIRCUIT_BREAKER__ENABLED`. These win over the file. Every setting's variable is in the tables below; list-valued settings take a comma-separated string.

Run `orion-server validate-config` to see the merged result without starting: it prints the full effective config — every section, serialized from the same structs the server runs on — as TOML (`--format json` and `--format summary` also exist). Secrets are masked with the same policy as the connector API: values under secret-looking keys are replaced with `******`, and passwords embedded in URL-shaped values such as `storage.url` are struck out in place. Configuration is validated at startup too, and an invalid value stops the boot rather than being silently ignored.

### Misspellings are startup errors, not silent no-ops

A key the config file does not have fails to parse and names itself — `[server] wrokers = 4` stops the boot. The environment is held to the same standard: Orion scans the process environment at startup and refuses any variable that follows the override grammar without being one of the overrides on this page, naming the offender and the nearest real key.

```
Error: Configuration error: these ORION_* environment variables are not Orion
settings and would be silently ignored:
  ORION_SERVER__PORTT (did you mean ORION_SERVER__PORT?)
```

**What the scan looks at is the `__`, not the `ORION_`.** Every override is `ORION_` + the field path with a double underscore between levels, so a name without one — `ORION_PORT`, `ORION_SERVER_URL` — cannot be a misspelling of a setting and is left alone. That matters because the prefix is not Orion's to claim: Kubernetes hands every pod a Docker-style block named after each Service in the namespace unless the PodSpec sets `enableServiceLinks: false`, so a Service called `orion` puts `ORION_SERVICE_HOST`, `ORION_PORT` and `ORION_PORT_8080_TCP_ADDR` into every container, written by the kubelet rather than by any manifest you can edit. `orion-cli` reads `ORION_SERVER_URL` and `ORION_API_KEY`, and a shell that exports those for the CLI passes them to a server started from the same shell. None of those can collide, and none needs a workaround. (The shipped Helm chart sets `enableServiceLinks: false` regardless — nothing here reads the link variables.)

The price is the single-underscore near-miss: `ORION_SERVER_PORT` is exactly the link a Service named `orion-server` would produce, so it is ignored rather than reported. Type the separator and the guard has your back.

Three exemptions cover names that *do* carry the separator, or would:

- **Names the config file references.** A file containing `url = "${ORION_DB_URL}"` makes `ORION_DB_URL` legitimate, even though no setting is named after it. The same `${VAR}` substitution also runs over connector `config_json` blobs — those live in the database and cannot be enumerated while the config is loading, so name them under `ORION_SECRET_*`.
- **The reserved `ORION_SECRET_*` namespace**, which Orion never interprets as configuration. Use it for values you reference yourself and cannot declare up front — an `env://ORION_SECRET_DB_PASSWORD` connector secret, for instance, since connectors live in the database rather than the config.
- **`ORION_ENVIRONMENT`**, the one setting that lives at the top level of the config and so has no separator of its own. It is checked by proximity instead: `ORION_ENVIRONMEN` is refused with a suggestion, because a silently ignored `environment` would leave the instance in `development` with the production checks downgraded to warnings.

One more `ORION_*` name is read but is **not** a config setting: `ORION_ADMIN_TOKEN` is the bearer token the [`orion-server package`](./cli.md#package) subcommands send when calling a target instance's admin API. It carries no `__`, so the startup scan leaves it alone, like the `ORION_SERVER_URL` / `ORION_API_KEY` pair `orion-cli` reads.

## Deployment Environment

One setting changes how strictly everything else is validated.

| Setting | Default | Env var | When to change |
|---|---|---|---|
| `environment` | `"development"` | `ORION_ENVIRONMENT` | Set to `"production"` before exposing an instance to anything you care about. |

Any value starting with `prod` (case-insensitive) is a production environment, which turns three warnings into startup errors:

- **Admin auth must be enabled.** `admin_auth.enabled = false` becomes a fatal config error instead of a log line nobody reads.
- **CORS may not be `["*"]`.** The wildcard is rejected; list explicit origins.
- **A cluster may not migrate at boot.** `cluster.enabled = true` with `storage.auto_migrate = true` is refused — see [`auto_migrate` in a cluster](#storage).

That is the whole mechanism — it does not change any other default. Everything else on this page is still yours to set, and the [Production Checklist](../operate/production-checklist.md) is the list worth walking.

The variable is `ORION_ENVIRONMENT`, derived from the field name like every other override. `ORION_ENV` was the pre-1.0 alias and is now refused at startup rather than silently ignored.

## Database Backend

The database backend is selected at runtime from the `storage.url` scheme. No rebuild needed:

| Backend | URL Format | Example |
|---------|------------|---------|
| **SQLite** | `sqlite:` | `sqlite:orion.db` or `sqlite::memory:` |
| **PostgreSQL** | `postgres://` | `postgres://user:pass@host/db` |
| **MySQL** | `mysql://` | `mysql://user:pass@host/db` |

```bash
# SQLite (default)
orion-server

# PostgreSQL
ORION_STORAGE__URL="postgres://user:pass@localhost/orion" orion-server
```

Migrations for all backends are embedded in the binary and the correct set is selected automatically at startup.

## Server

| Setting | Default | Env var | When to change |
|---|---|---|---|
| `server.host` | `"0.0.0.0"` | `ORION_SERVER__HOST` | Bind to `127.0.0.1` when a local proxy is the only intended client. |
| `server.port` | `8080` | `ORION_SERVER__PORT` | To fit an existing port convention. |
| `server.shutdown_drain_secs` | `30` | `ORION_SERVER__SHUTDOWN_DRAIN_SECS` | Raise it if your slowest request legitimately outlives 30 s, so rolling deploys stop cutting them off. |
| `server.shutdown_force_timeout_secs` | `30` | `ORION_SERVER__SHUTDOWN_FORCE_TIMEOUT_SECS` | Hard cap on waiting after the drain window. `0` waits forever — only with an orchestrator that will eventually SIGKILL. |
| `server.max_admin_body_size` | `8388608` | `ORION_SERVER__MAX_ADMIN_BODY_SIZE` | Raise for very large bulk imports or workflow exports. Applies to `/api/v1/admin/*` only; the data plane keeps `ingest.max_payload_size`. |
| `server.verbose_errors` | — | `ORION_SERVER__VERBOSE_ERRORS` | Unset returns real task-failure messages on the data plane when `environment` is not a production variant, and the generic placeholder in it. `false` sanitizes everywhere. **`true` is refused in production** and the server will not start. |

On SIGTERM or SIGINT Orion withdraws readiness first, then stops accepting, then drains. Set both timeouts below your orchestrator's termination grace period, or it kills the process mid-drain.

A failed task answers `200` with the failure in the envelope's `errors` array,
carrying the task's `code` and `task_id` either way — `verbose_errors` decides
only whether `message` is the engine's own text or the placeholder. Full detail
is always in the persisted trace, correlated by the `request_id` the sanitized
envelope adds.

### TLS

Terminate HTTPS in Orion itself, or leave this off when a load balancer or service mesh already terminates TLS.

| Setting | Default | Env var | When to change |
|---|---|---|---|
| `server.tls.enabled` | `false` | `ORION_SERVER__TLS__ENABLED` | Enable when Orion is directly reachable by clients. |
| `server.tls.cert_path` | `""` | `ORION_SERVER__TLS__CERT_PATH` | PEM certificate chain. Required when enabled. |
| `server.tls.key_path` | `""` | `ORION_SERVER__TLS__KEY_PATH` | PEM private key. Required when enabled. |

```toml
[server.tls]
enabled = true
cert_path = "/etc/orion/tls/tls.crt"
key_path = "/etc/orion/tls/tls.key"
```

Both files must exist and be readable at startup; Orion refuses to boot otherwise rather than falling back to plain HTTP.

### Compression

| Setting | Default | Env var | When to change |
|---|---|---|---|
| `server.compression.enabled` | `false` | `ORION_SERVER__COMPRESSION__ENABLED` | Enable when responses are typically large. |

Off by default because the layer is unconditional once inserted: it runs DEFLATE on every response regardless of size, which costs CPU without saving bytes on small JSON bodies (a ~100 B response can grow slightly after gzip overhead).

### API docs

| Setting | Default | Env var | When to change |
|---|---|---|---|
| `server.docs.enabled` | — | `ORION_SERVER__DOCS__ENABLED` | Unset serves Swagger UI (`/docs`) and the spec (`/api/v1/openapi.json`) only when `environment` is not a production variant. Set `true` to serve them in production anyway, `false` to switch them off everywhere. |

Both endpoints are unauthenticated and the spec publishes the complete admin API surface — route shapes, request schemas, the `admin_auth.header` semantics — so production deployments do not serve them by default. When disabled the routes are not registered at all: both paths return 404, not 401, so their existence is not advertised. `orion-server dump-openapi` writes the spec to a file offline regardless of this setting.

## Storage

| Setting | Default | Env var | When to change |
|---|---|---|---|
| `storage.url` | `"sqlite:orion.db"` | `ORION_STORAGE__URL` | Point at PostgreSQL or MySQL for anything multi-replica or high-write. |
| `storage.max_connections` | `50` | `ORION_STORAGE__MAX_CONNECTIONS` | **Size against the database's own limit** — see below. |
| `storage.min_connections` | `5` | `ORION_STORAGE__MIN_CONNECTIONS` | Raise to keep more connections warm under bursty traffic; `0` keeps none. |
| `storage.busy_timeout_ms` | `5000` | `ORION_STORAGE__BUSY_TIMEOUT_MS` | SQLite only — raise under heavy concurrent writes. Ignored by other backends. |
| `storage.acquire_timeout_secs` | `3` | `ORION_STORAGE__ACQUIRE_TIMEOUT_SECS` | How long a request waits for a free pooled connection before failing. Lower it to shed load faster; raise it only if brief pool exhaustion is expected and acceptable. |
| `storage.idle_timeout_secs` | `300` | `ORION_STORAGE__IDLE_TIMEOUT_SECS` | Lower it when a proxy (PgBouncer, RDS Proxy) closes idle connections sooner; `0` never closes them. |
| `storage.connector_encryption_key` | `""` | `ORION_STORAGE__CONNECTOR_ENCRYPTION_KEY` | Encrypt `connectors.config_json` at rest (AES-256-GCM). Empty = plaintext. 64-hex key (`openssl rand -hex 32`); prefer the env var. Pre-existing plaintext rows keep loading and re-encrypt on their next write. |
| `storage.backup_dir` | `"./backups"` | `ORION_STORAGE__BACKUP_DIR` | Where `POST /api/v1/admin/backups` writes. SQLite only. |
| `storage.backup_retention_count` | — | `ORION_STORAGE__BACKUP_RETENTION_COUNT` | Keep only the newest N backups, pruning older ones after each successful backup. Unset keeps every backup — they accumulate on the same disk as the live database. Set the variable to an empty string to clear it. |
| `storage.auto_migrate` | `true` | `ORION_STORAGE__AUTO_MIGRATE` | **Set `false` for multi-replica deployments** and run `orion-server migrate` as a deploy step — required in a production cluster. |
| `storage.connect_retry_secs` | `60` | `ORION_STORAGE__CONNECT_RETRY_SECS` | How long startup keeps retrying an unreachable database before giving up (`0` = fail fast). Sized so a pod rides out a Postgres/MySQL failover instead of crash-looping through it. Ignored for SQLite, whose connect failures are not transient; the `auto_migrate = false` pending-migration check stays fail-fast regardless. |

**Use SQLite for:** a single instance — a development or design-time node, an appliance install, anything where one process owns the database file. It is the default, provisions nothing, and creates the file on first boot. It is also the only backend with an [in-product backup](../operate/backup-restore.md#what-is-provided-per-backend).

**Use PostgreSQL or MySQL for:** more than one replica, write-heavy estates, and every deployment that needs [cluster mode](../operate/cluster.md#requirements) — which refuses to start against a `sqlite:` URL, because a file is single-host. Backup and restore become your snapshot or PITR tooling's job; Orion provides neither for these backends.

**Sizing the pool.** `max_connections` is per process. With N replicas, N × `max_connections` must stay below the server's own `max_connections` (PostgreSQL's default is 100, and superuser slots and other clients come out of that budget) or replicas will fail to connect under load. The default of 50 suits a single node against a dedicated database; three replicas against a stock Postgres want roughly 25 each, less whatever else connects.

**`auto_migrate` in a cluster.** With `auto_migrate = true`, every replica tries to migrate at boot and they race. The intended shape is `auto_migrate = false` plus `orion-server migrate` as a pre-deploy job; startup then fails fast if migrations are still pending, instead of serving against a schema it does not understand.

`cluster.enabled = true` with `auto_migrate = true` is **a startup error in production** — a guardrail that fires after the race is no guardrail. Outside production it stays a warning, which is what lets a throwaway cluster (the Helm chart's `devStack`, whose database is created by the same release and so cannot be migrated by a pre-install hook) boot without a migrate step. A single-node install is unaffected either way: `cluster.enabled` is `false` by default, and migrating at boot is what makes `orion-server` a single-binary install.

The migrate step already exists in both reference deployments: the Helm chart runs a pre-install/pre-upgrade `orion-server migrate` Job, and `docker-compose.ha.yml` a one-shot `migrate` service the replicas depend on.

## Cluster (HA)

With `cluster.enabled = false` — the default — Orion is a plain single node: no epoch watcher, no shared backends, no job leases. Enable it and N replicas sharing one PostgreSQL/MySQL and one Redis behave as a single logical system:

- **Config changes propagate.** A workflow or channel edited through any node bumps a database epoch; every other node notices within `epoch_poll_interval_ms` and reloads. Without this, an edit only affects the replica that received it.
- **Dedup and response caches default to the shared Redis**, so idempotency and caching are fleet-wide rather than per-node. A channel whose dedup store would silently degrade to node-local memory refuses to load instead.
- **Rate-limit windows are shared**, so a channel's limit is the fleet's limit and not N times it.
- **Background jobs single-flight.** Trace cleanup, audit cleanup, and DLQ retry run on one node at a time behind a lease.

| Setting | Default | Env var | When to change |
|---|---|---|---|
| `cluster.enabled` | `false` | `ORION_CLUSTER__ENABLED` | Turn on whenever more than one Orion process serves the same database. |
| `cluster.redis_url` | `""` | `ORION_CLUSTER__REDIS_URL` | Required when enabled, e.g. `redis://redis:6379`. |
| `cluster.epoch_poll_interval_ms` | `2000` | `ORION_CLUSTER__EPOCH_POLL_INTERVAL_MS` | Lower for faster config propagation, at the cost of more database polling. |
| `cluster.instance_id` | `""` | `ORION_CLUSTER__INSTANCE_ID` | Set a stable per-replica value (e.g. the pod name) so Kafka static membership survives restarts. Empty generates a UUID per boot. |

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

## Ingest

| Setting | Default | Env var | When to change |
|---|---|---|---|
| `ingest.max_payload_size` | `1048576` | `ORION_INGEST__MAX_PAYLOAD_SIZE` | Raise for large request bodies on the **data plane**. The admin API has its own bound (`server.max_admin_body_size`), so raising this one does not widen the unauthenticated surface. |

## Engine

| Setting | Default | Env var | When to change |
|---|---|---|---|
| `engine.health_check_timeout_secs` | `2` | `ORION_ENGINE__HEALTH_CHECK_TIMEOUT_SECS` | Rarely — it bounds the `/readyz` cluster-Redis `PING` (the engine itself is lock-free and needs no health window). |
| `engine.max_channel_call_depth` | `10` | `ORION_ENGINE__MAX_CHANNEL_CALL_DEPTH` | Lower it to catch accidental recursion between channels sooner. |
| `engine.default_channel_call_timeout_ms` | `30000` | `ORION_ENGINE__DEFAULT_CHANNEL_CALL_TIMEOUT_MS` | Default deadline for `channel_call` when the task sets none. |
| `engine.global_http_timeout_secs` | `30` | `ORION_ENGINE__GLOBAL_HTTP_TIMEOUT_SECS` | Safety net for every outbound HTTP request; shorter connector or task timeouts still win. |
| `engine.max_pool_cache_entries` | `100` | `ORION_ENGINE__MAX_POOL_CACHE_ENTRIES` | Raise only with more than ~100 distinct external connectors. LRU-evicted. |
| `engine.cache_cleanup_interval_secs` | `60` | `ORION_ENGINE__CACHE_CLEANUP_INTERVAL_SECS` | Sweep interval for expired in-memory cache entries. |
| `engine.max_memory_cache_entries` | `100000` | `ORION_ENGINE__MAX_MEMORY_CACHE_ENTRIES` | Per-namespace bound — see below. Lower it on a memory-constrained host. `0` removes the bound. |
| `engine.rollout_sticky_header` | `""` | `ORION_ENGINE__ROLLOUT_STICKY_HEADER` | Set to the header that identifies a caller (e.g. `"x-user-id"`) so canary rollouts are stable per caller. |
| `engine.fail_on_connector_load_error` | `false` | `ORION_ENGINE__FAIL_ON_CONNECTOR_LOAD_ERROR` | **Set to `true` in production.** Refuse to start when an enabled connector cannot be loaded — see below. |

### Connector load failures

An enabled connector whose config cannot be loaded — a missing `env://DB_PASSWORD`, an unparseable `config_json`, an unresolvable secret reference — is skipped. It is then simply *absent*: every workflow using it returns a 500 at request time, which may be hours after the deploy that broke it.

Three surfaces report this:

- `GET /health` sets `components.connectors` to `degraded` and lists the failures under `connectors.failed_to_load`. The overall status becomes `degraded`, but the HTTP status stays **200** — the rest of the instance is serving, and a 503 would pull the node out of its load balancer over a connector nothing in flight may be using. Alert on the field, not the status code.
- `GET /api/v1/admin/connectors` gives every row a `load_status` of `loaded`, `failed`, or `disabled`, with `load_error` and `load_error_stage` on the failures.
- `engine.fail_on_connector_load_error = true` refuses to start at all, so a bad rollout fails where the orchestrator will catch it. This is startup only — a hot reload never takes a running process down.

**`max_memory_cache_entries`** bounds each in-memory cache **namespace**, with LRU eviction on insert. There is no single shared store: the built-in dedup store, the built-in response cache, and every `(purpose, connector)` use of a `backend = "memory"` cache connector each get their own instance with their own bound, so a hot workflow cache cannot evict dedup entries — but the budgets add up. Worst-case resident entries are `max_memory_cache_entries × number of namespaces`: the two built-in stores plus up to three (workflow cache, dedup, response cache) for every memory connector. Size a memory-constrained host from that product, not from the single value. Setting `0` disables the bound, at which point entries written without a TTL are never reclaimed; only do that when the key set is known to be finite.

**`rollout_sticky_header`** decides how a request is bucketed for canary rollouts. With a header configured, the same caller always lands in the same bucket and therefore on the same workflow version. Empty (the default) falls back to the forwarded client IP, and with neither available the bucket is random per request — so a caller can flip between versions mid-session.

### Circuit Breaker

Fields below configure the global breaker; trip conditions and per-instance
behaviour are specified in
[Connector Types › Circuit breakers](./connectors.md#circuit-breakers).

Sheds load to a failing dependency: after `failure_threshold` consecutive failures the breaker opens and calls return `503 CIRCUIT_OPEN` immediately, until `recovery_timeout_secs` elapses and a probe is admitted.

| Setting | Default | Env var | When to change |
|---|---|---|---|
| `engine.circuit_breaker.enabled` | `false` | `ORION_ENGINE__CIRCUIT_BREAKER__ENABLED` | Enable in production whenever workflows call external HTTP services. |
| `engine.circuit_breaker.failure_threshold` | `5` | `ORION_ENGINE__CIRCUIT_BREAKER__FAILURE_THRESHOLD` | Lower to trip sooner on a flaky dependency; raise to tolerate isolated errors. |
| `engine.circuit_breaker.recovery_timeout_secs` | `30` | `ORION_ENGINE__CIRCUIT_BREAKER__RECOVERY_TIMEOUT_SECS` | How long the breaker stays open before probing. |
| `engine.circuit_breaker.max_breakers` | `10000` | `ORION_ENGINE__CIRCUIT_BREAKER__MAX_BREAKERS` | Rarely — bounds the tracked `channel:connector` pairs before LRU eviction. |

Breakers are keyed per channel and connector, so one noisy channel does not trip a shared connector for everyone else, and the state is per node. Currently applied to `http_call`.

## Trace Queue

The async trace pipeline: `POST /{channel}/async` enqueues, workers execute, and failures land in a database dead-letter queue with automatic retry.

| Setting | Default | Env var | When to change |
|---|---|---|---|
| `trace_queue.workers` | `4` | `ORION_TRACE_QUEUE__WORKERS` | Raise for more concurrent async processing. This is the real worker knob. |
| `trace_queue.buffer_size` | `1000` | `ORION_TRACE_QUEUE__BUFFER_SIZE` | Raise to absorb bigger bursts before submissions are rejected. |
| `trace_queue.shutdown_timeout_secs` | `30` | `ORION_TRACE_QUEUE__SHUTDOWN_TIMEOUT_SECS` | How long shutdown waits for in-flight traces. |
| `trace_queue.retention_hours` | `72` | `ORION_TRACE_QUEUE__RETENTION_HOURS` | Lower to shrink the `traces` table; `0` keeps traces forever. |
| `trace_queue.cleanup_interval_secs` | `3600` | `ORION_TRACE_QUEUE__CLEANUP_INTERVAL_SECS` | How often the trace cleanup job runs. |
| `trace_queue.processing_timeout_ms` | `60000` | `ORION_TRACE_QUEUE__PROCESSING_TIMEOUT_MS` | Per-trace deadline on the async path. |
| `trace_queue.max_result_size_bytes` | `1048576` | `ORION_TRACE_QUEUE__MAX_RESULT_SIZE_BYTES` | Raise for large results; oversized ones are rejected (sync) or failed (async). |
| `trace_queue.max_queue_memory_bytes` | `104857600` | `ORION_TRACE_QUEUE__MAX_QUEUE_MEMORY_BYTES` | Total queued payload bytes before new submissions get `503`. |
| `trace_queue.dlq_retry_enabled` | `true` | `ORION_TRACE_QUEUE__DLQ_RETRY_ENABLED` | Disable only to freeze the DLQ for inspection — note the `orion_trace_dlq_depth` gauge stops updating with it. |
| `trace_queue.dlq_max_retries` | `5` | `ORION_TRACE_QUEUE__DLQ_MAX_RETRIES` | Attempts before a row is marked exhausted. Must be 1–16 (backoff is 2^retries seconds); use `dlq_retry_enabled` to turn retries off. |
| `trace_queue.dlq_poll_interval_secs` | `30` | `ORION_TRACE_QUEUE__DLQ_POLL_INTERVAL_SECS` | How often the retry worker polls. |
| `trace_queue.dlq_batch_size` | `20` | `ORION_TRACE_QUEUE__DLQ_BATCH_SIZE` | Rows claimed per retry tick. Raise to drain a large backlog faster. |
| `trace_queue.dlq_lease_secs` | `60` | `ORION_TRACE_QUEUE__DLQ_LEASE_SECS` | How long a claimed row stays leased to one node. |

## Audit Log Retention

| Setting | Default | Env var | When to change |
|---|---|---|---|
| `audit.retention_days` | `90` | `ORION_AUDIT__RETENTION_DAYS` | Raise to satisfy a retention policy; `0` keeps rows forever. |
| `audit.cleanup_interval_secs` | `3600` | `ORION_AUDIT__CLEANUP_INTERVAL_SECS` | How often the audit cleanup job runs. |
| `audit.max_pending` | `1000` | `ORION_AUDIT__MAX_PENDING` | Audit rows accepted but not yet written. Raise on a bursty admin plane; a full queue drops rows and counts them in `orion_audit_events_dropped_total{reason="queue_full"}`. |
| `audit.drain_timeout_secs` | `5` | `ORION_AUDIT__DRAIN_TIMEOUT_SECS` | How long shutdown waits for the audit queue to drain before abandoning what is left (and saying how much). Rejected at startup if `0` — unlike the other timeouts on this page, zero is not "no bound" here, it would skip the drain. |

**Audit retention.** Every admin mutation writes an `audit_logs` row and nothing else removes them, so `audit.retention_days = 0` grows that table without bound. Before 1.0 these settings lived in `[queue]` and the cleanup job borrowed the trace job's cadence; they now have their own section and their own interval.

**Audit durability.** Admin responses never wait on the audit INSERT — the row
goes onto a bounded queue that one background writer drains in order. That
queue is drained at shutdown (bounded by `audit.drain_timeout_secs`), so a
mutation accepted moments before `SIGTERM` is still recorded; before 1.0 the
write was a detached task and that row was simply lost. Any row that does not
make it is counted in `orion_audit_events_dropped_total` and logged at `error`
— alert on that counter being non-zero at all rather than on a threshold.

**DLQ leases.** A claimed row is leased for `dlq_lease_secs`; when the lease expires another node may re-claim it. That is how work from a crashed node is recovered in cluster mode, so the value should comfortably exceed how long one retry takes.

## Query and Write Bounds

Safety bounds for the portable `data_query` / `data_write` handlers. Requests over a bound are rejected, never silently clamped or truncated.

| Setting | Default | Env var | When to change |
|---|---|---|---|
| `query.default_limit` | `100` | `ORION_QUERY__DEFAULT_LIMIT` | Page size applied when a query omits `limit`. |
| `query.max_limit` | `1000` | `ORION_QUERY__MAX_LIMIT` | Hard cap on page size. Must be ≥ `default_limit`. |
| `query.max_skip` | `10000` | `ORION_QUERY__MAX_SKIP` | Hard cap on the `skip` offset, enforced on every backend. A query skipping more is rejected, never clamped. |
| `write.max_rows` | `1000` | `ORION_WRITE__MAX_ROWS` | Hard cap on rows per bulk insert or upsert. |
| `write.allow_unfiltered` | `false` | `ORION_WRITE__ALLOW_UNFILTERED` | Leave `false` unless a workflow genuinely needs unfiltered `update`/`delete` — which still also requires `"all": true` on the call itself. |

**`include` pages too.** `query.default_limit` and `query.max_limit` govern an `include`'s nested page as well, applied **per parent**: an `include` with no `limit` fetches `default_limit` children *for each parent row*, and one above `max_limit` is rejected. A page of 100 parents with an unbounded-looking `include` is therefore bounded at `100 × default_limit` child rows — size the two together.

## Kafka

Consumer and producer are compiled into every binary and gated at runtime by `kafka.enabled`.

| Setting | Default | Env var | When to change |
|---|---|---|---|
| `kafka.enabled` | `false` | `ORION_KAFKA__ENABLED` | Enable to consume from Kafka topics. |
| `kafka.brokers` | `["localhost:9092"]` | `ORION_KAFKA__BROKERS` | Comma-separated in the env var. Each entry must be `host:port`. |
| `kafka.group_id` | `"orion"` | `ORION_KAFKA__GROUP_ID` | Give each deployment its own group so they do not share offsets. |
| `kafka.topics` | `[]` | `ORION_KAFKA__TOPICS` | Topic-to-channel mappings — see below. |
| `kafka.processing_timeout_ms` | `60000` | `ORION_KAFKA__PROCESSING_TIMEOUT_MS` | Per-message deadline. |
| `kafka.lag_poll_interval_secs` | `30` | `ORION_KAFKA__LAG_POLL_INTERVAL_SECS` | `0` disables consumer-lag metrics. |
| `kafka.session_timeout_ms` | `45000` | `ORION_KAFKA__SESSION_TIMEOUT_MS` | Consumer group session timeout; applied whether or not cluster mode is on. In cluster mode it pairs with static group membership (`group.instance.id`) so rolling restarts rejoin without a full rebalance. |

Messages are processed **strictly sequentially** per consumer — the at-least-once delivery contract requires it, because committing an offset implicitly commits every earlier offset on that partition. Scale throughput by running more instances in the same consumer group. (The pre-1.0 `kafka.max_inflight` setting is gone; it advertised concurrency that never existed.)

Topic mappings are TOML array-of-tables, and channels with a Kafka protocol contribute their own topics from the database — the two sets are merged at startup:

```toml
[[kafka.topics]]
topic = "incoming-orders"
channel = "orders"
```

The env-var form is a comma-separated `topic:channel` list: `ORION_KAFKA__TOPICS="incoming-orders:orders,events:event-handler"`.

### Dead-Letter Queue

| Setting | Default | Env var | When to change |
|---|---|---|---|
| `kafka.dlq.enabled` | `false` | `ORION_KAFKA__DLQ__ENABLED` | Enable so poison messages stop blocking a partition. |
| `kafka.dlq.topic` | `"orion-dlq"` | `ORION_KAFKA__DLQ__TOPIC` | To match an existing naming convention. |

Delivery is at-least-once: an offset advances only on successful processing or a *confirmed* DLQ write. With the DLQ disabled a failing message is retried in place with capped backoff rather than lost — which is safe, but means one poison message can stall its partition until you enable this.

### Broker Authentication

This is what makes managed brokers reachable — Confluent Cloud, MSK, Aiven. Settings apply to every Kafka client Orion creates: the ingest consumer, the `publish_kafka` producer, and the DLQ producer. Each maps 1:1 onto a librdkafka property, and an unset field leaves librdkafka's default (plaintext, no auth) alone.

| Setting | Default | Env var | When to change |
|---|---|---|---|
| `kafka.auth.security_protocol` | — | `ORION_KAFKA__AUTH__SECURITY_PROTOCOL` | `plaintext`, `ssl`, `sasl_plaintext`, or `sasl_ssl`. Any managed broker needs `sasl_ssl`. |
| `kafka.auth.sasl_mechanism` | — | `ORION_KAFKA__AUTH__SASL_MECHANISM` | `PLAIN`, `SCRAM-SHA-256`, or `SCRAM-SHA-512`. |
| `kafka.auth.sasl_username` | — | `ORION_KAFKA__AUTH__SASL_USERNAME` | The API key on Confluent Cloud. |
| `kafka.auth.sasl_password` | — | `ORION_KAFKA__AUTH__SASL_PASSWORD` | The API secret. Prefer the env var or a `${VAR}` placeholder over a literal. |
| `kafka.auth.ssl_ca_location` | — | `ORION_KAFKA__AUTH__SSL_CA_LOCATION` | Path to a CA bundle for broker verification; unset uses the system trust store. |

Choosing a `security_protocol` starting with `sasl` requires `sasl_mechanism`, `sasl_username`, and `sasl_password` — startup fails if any is missing, rather than falling back to an unauthenticated connection. GSSAPI and OAUTHBEARER are not available: librdkafka is built without libsasl2.

Confluent Cloud, end to end:

```toml
[kafka]
enabled = true
brokers = ["pkc-abc12.us-east-1.aws.confluent.cloud:9092"]
group_id = "orion-prod"

[[kafka.topics]]
topic = "orders"
channel = "order-processor"

[kafka.auth]
security_protocol = "sasl_ssl"
sasl_mechanism = "PLAIN"
sasl_username = "${CONFLUENT_API_KEY}"
sasl_password = "${CONFLUENT_API_SECRET}"
```

AWS MSK with IAM authentication is not supported; use MSK's SCRAM credentials with `sasl_mechanism = "SCRAM-SHA-512"`.

### Raw librdkafka Properties

| Setting | Default | Env var | When to change |
|---|---|---|---|
| `kafka.extra_config` | — | — | For any librdkafka property Orion has no first-class setting for. |

Applied to every client *after* everything Orion sets, so entries here override anything — including `[kafka.auth]`. Free-form maps do not fit the `ORION_SECTION__KEY` scheme, so there is no environment variable: this is config file only.

```toml
[kafka.extra_config]
"client.id" = "orion-prod-1"
"socket.keepalive.enable" = "true"
```

## Rate Limiting

Platform-level limits, applied per client identity. Per-channel limits are separate and live in the channel's `config_json` in the database.

| Setting | Default | Env var | When to change |
|---|---|---|---|
| `rate_limit.enabled` | `false` | `ORION_RATE_LIMIT__ENABLED` | Enable on any internet-facing instance. |
| `rate_limit.default_rps` | `100` | `ORION_RATE_LIMIT__DEFAULT_RPS` | Sustained requests per second per client. |
| `rate_limit.default_burst` | `50` | `ORION_RATE_LIMIT__DEFAULT_BURST` | Burst allowance above the sustained rate. |
| `rate_limit.trusted_proxies` | `[]` | `ORION_RATE_LIMIT__TRUSTED_PROXIES` | **Set this if Orion sits behind a load balancer**, whether or not `rate_limit.enabled` is on — see below. |
| `rate_limit.endpoints.admin_rps` | `20` | `ORION_RATE_LIMIT__ENDPOINTS__ADMIN_RPS` | Separate limit for the admin API. Set the variable to an empty string to clear it, which makes the admin plane use `default_rps`. |
| `rate_limit.endpoints.data_rps` | — | `ORION_RATE_LIMIT__ENDPOINTS__DATA_RPS` | Separate limit for the data plane; unset means it uses `default_rps`. Set the variable to an empty string to clear it. |

**`trusted_proxies` changes behaviour for every proxied deployment.** The direct peer IP is authoritative. `X-Forwarded-For` and `X-Real-IP` are honoured *only* when the peer address falls inside one of these CIDR blocks (bare IPs are accepted and treated as `/32` or `/128`). The default is empty, which means **forwarded headers are never trusted**. When the peer is trusted, the client is the **rightmost** `X-Forwarded-For` hop that is not itself a trusted proxy — the hop your own proxy appended. The leftmost elements arrive from the client verbatim and are never used, so a forged prefix cannot mint an identity.

The consequence in both directions:

- **Behind a load balancer with this unset**, every request appears to come from the balancer, so all clients share a single rate-limit bucket and the limit effectively applies to your whole fleet at once. List the balancer's subnet — `trusted_proxies = ["10.0.0.0/8"]` — to get per-client limiting back.
- **List a network you do not control** and clients on it can spoof `X-Forwarded-For` to mint a fresh bucket per request, which is exactly no rate limiting at all. List only the addresses of proxies you operate.

**It applies even with `rate_limit.enabled = false`.** The key is spelled under `[rate_limit]`, but what it configures is "may a forwarded header name the client", and four consumers resolve the caller's address with it: the platform rate limiter, the failed-admin-auth backoff, audit rows' `client_ip`, and per-channel `rate_limit` blocks. The full reasoning is in [Design Notes › Why forwarded headers are ignored by default](./design-notes.md#why-forwarded-headers-are-ignored-by-default).

Both endpoint limits are optional, so their environment variables are three-state: unset leaves the config-file value alone, a number sets the limit, and an empty string clears it back to "use `default_rps`".

## Channel Filter

| Setting | Default | Env var | When to change |
|---|---|---|---|
| `channel_filter.include` | `[]` | `ORION_CHANNEL_FILTER__INCLUDE` | Glob patterns; empty loads every active channel. |
| `channel_filter.exclude` | `[]` | `ORION_CHANNEL_FILTER__EXCLUDE` | Applied after `include`. |

Both are matched against the channel name and are comma-separated in the env var. Use them to run separate fleets off one database — a public instance serving `orders-*` and an internal one serving the rest — without splitting the control plane.

## Admin Authentication

Guards `/api/v1/admin/*` and the trace-read endpoints. Disabled by default so a fresh install is usable; **required** once `environment` starts with `prod` — startup fails without it.

| Setting | Default | Env var | When to change |
|---|---|---|---|
| `admin_auth.enabled` | `false` | `ORION_ADMIN_AUTH__ENABLED` | Enable anywhere the admin API is reachable by anything but you. |
| `admin_auth.api_keys` | `[]` | `ORION_ADMIN_AUTH__API_KEYS` | Comma-separated in the env var. Any listed key authorises a request. |
| `admin_auth.read_only_api_keys` | `[]` | `ORION_ADMIN_AUTH__READ_ONLY_API_KEYS` | Keys limited to `GET`/`HEAD`; mutating methods answer `403`. For dashboards, auditors and CI checks. |
| `admin_auth.header` | `"Authorization"` | `ORION_ADMIN_AUTH__HEADER` | `"Authorization"` expects `Bearer <key>`; any other value (e.g. `"X-API-Key"`) expects the raw key. |

**Multiple keys exist for rotation.** Add the new key, roll clients over, then drop the old one — no restart gap where a valid client is refused.

**Keys may be stored hashed.** Each entry is either the plaintext key or `sha256:<64-hex>`, the SHA-256 digest of the key, so the config file and any snapshot of it hold a hash rather than a usable secret. Both forms verify the same presented token, and requests are compared at fixed width. Generate a digest with:

```bash
printf %s "$MY_ADMIN_KEY" | shasum -a 256
```

```toml
[admin_auth]
enabled = true
api_keys = ["sha256:9f86d081884c7d659a2feaa0c55ad015a3bf4f1b2b0b822cd15d6c15b0f00a08"]
```

A malformed `sha256:` entry is a startup error, not a key that silently never matches. Audit entries record a hash prefix for hashed keys, so you can tell which key performed a mutation without storing the key.

## CORS

| Setting | Default | Env var | When to change |
|---|---|---|---|
| `cors.allowed_origins` | `["*"]` | `ORION_CORS__ALLOWED_ORIGINS` | List explicit origins before production; comma-separated in the env var. |

Exactly `["*"]` is permissive CORS and is **rejected at startup** when `environment` starts with `prod`. Mixing `"*"` into a list of explicit origins is always a config error — it used to pass validation and then panic at router build.

## Logging and Metrics

| Setting | Default | Env var | When to change |
|---|---|---|---|
| `logging.level` | `"info"` | `ORION_LOGGING__LEVEL` | `trace`, `debug`, `info`, `warn`, `error`. `RUST_LOG=orion=debug` gives per-crate control. |
| `logging.format` | `"pretty"` | `ORION_LOGGING__FORMAT` | `json` wherever logs are collected by anything other than a human. |
| `metrics.enabled` | `false` | `ORION_METRICS__ENABLED` | Enable to collect metrics and serve them at `GET /metrics`. With this off the route is not registered at all — `/metrics` 404s rather than answering `200` with a permanently empty body. |
| `metrics.bind_addr` | — | `ORION_METRICS__BIND_ADDR` | A dedicated `host:port` for an **unauthenticated** listener serving only `GET /metrics`. Unset keeps the endpoint on the main listener, where `admin_auth` guards it. Requires `metrics.enabled = true`; set on its own it raises no listener and startup warns. Refused at startup if it would contend with `server.host`/`server.port` — same port counts as contention whenever either side is a wildcard address. |

**Where `/metrics` is served.** By default it lives on the main listener, so
with `admin_auth.enabled = true` every scraper must hold an admin API key — a
credential that can also rewrite workflows and read trace payloads. Setting
`metrics.bind_addr` moves the endpoint onto its own listener, removes it from
the main one entirely, and drops the credential requirement:

```toml
[metrics]
enabled = true
bind_addr = "127.0.0.1:9090"     # or a pod IP / a private Compose network
```

That listener is plain HTTP (`server.tls` governs the main listener only) and
has no authentication by design, so the address is the access control — bind
it somewhere only your scrapers can reach. Startup logs a warning if it is not
a loopback address, and another if `bind_addr` is set while `metrics.enabled`
is `false` — that combination serves `/metrics` nowhere at all. It joins the same graceful-shutdown path as the main
listener, keeping the last scrape of a draining node available for
`server.shutdown_drain_secs`.

## Tracing

OpenTelemetry export, compiled into every binary and gated at runtime.

| Setting | Default | Env var | When to change |
|---|---|---|---|
| `tracing.enabled` | `false` | `ORION_TRACING__ENABLED` | Enable to export spans to an OTLP collector. |
| `tracing.otlp_endpoint` | `"http://localhost:4317"` | `ORION_TRACING__OTLP_ENDPOINT` | Point at your collector (Jaeger, Tempo, OTel Collector). |
| `tracing.service_name` | `"orion"` | `ORION_TRACING__SERVICE_NAME` | Distinguish multiple Orion deployments in one backend. |
| `tracing.sample_rate` | `1.0` | `ORION_TRACING__SAMPLE_RATE` | Lower under high traffic; `0.0` to `1.0`. |
| `tracing.debug_profile_enabled` | `false` | `ORION_TRACING__DEBUG_PROFILE_ENABLED` | Leave off in production. |

With `debug_profile_enabled = true`, a request carrying `X-Orion-Profile: 1` (or `?profile=1`) gets an `_orion.profile` object breaking the request down by phase — engine lock wait, per-handler durations, trace store, residual workflow logic. It is off by default so callers cannot probe internal timing.

## Trace Persistence

Orion's own per-request trace records — rows in the `traces` table, read via `/api/v1/admin/traces`. Unrelated to the OTLP export in `[tracing]` above; before 1.0 these keys lived under `[tracing.storage]`, which is exactly the confusion the split removes. A channel can override the mode with its `config.tracing` field; unset per-channel fields fall back to what is set here.

| Mode | Behaviour |
|---|---|
| `sync` | Write inline before responding. Strongest durability; throughput capped by single-writer contention. |
| `async` | Enqueue to a bounded background queue, one database write per task. |
| `batch` | Bounded queue; workers commit `batch_size` rows per transaction. Highest throughput. |
| `off` | No persistence at all. |

| Setting | Default | Env var | When to change |
|---|---|---|---|
| `trace_storage.mode` | `"sync"` | `ORION_TRACE_STORAGE__MODE` | A served request implies a persisted trace, at the cost of the DB's write rate capping throughput. Set `batch` or `async` to lift that cap — the request path then runs ahead of the trace table and can overrun `max_pending`, at which point traces are shed per `async_on_overflow`. |
| `trace_storage.sample_rate` | `1.0` | `ORION_TRACE_STORAGE__SAMPLE_RATE` | Fraction of traces persisted, `0.0` to `1.0`. Applies to sync traces only — an async submission's trace row is how its result is delivered, so async traces always persist regardless of this rate; bound async storage with `errors_only` or `trace_queue.retention_hours` instead. |
| `trace_storage.errors_only` | `false` | `ORION_TRACE_STORAGE__ERRORS_ONLY` | Persist only traces that ended with errors — a cheap way to keep the table small. |
| `trace_storage.max_pending` | `10000` | `ORION_TRACE_STORAGE__MAX_PENDING` | Queue capacity in `async` and `batch` modes. |
| `trace_storage.async_on_overflow` | `"drop"` | `ORION_TRACE_STORAGE__ASYNC_ON_OVERFLOW` | `drop` or `block`. `block` applies backpressure to the request path. |
| `trace_storage.overflow_block_timeout_ms` | `100` | `ORION_TRACE_STORAGE__OVERFLOW_BLOCK_TIMEOUT_MS` | How long `block` waits for capacity before dropping anyway. |
| `trace_storage.async_workers` | `4` | `ORION_TRACE_STORAGE__ASYNC_WORKERS` | Worker count in `async` mode. |
| `trace_storage.batch_size` | `1000` | `ORION_TRACE_STORAGE__BATCH_SIZE` | Rows per transaction in `batch` mode, and the dominant term in how fast the queue drains: measured on SQLite with 4 workers, `100` drains 26k rows/s and `1000` drains 45k rows/s. Max 1000 — the batch INSERT binds ~11 parameters per row against SQLite's 32 766-bind statement cap. |
| `trace_storage.batch_flush_interval_ms` | `100` | `ORION_TRACE_STORAGE__BATCH_FLUSH_INTERVAL_MS` | How long a partial batch waits before flushing. |
| `trace_storage.batch_workers` | `4` | `ORION_TRACE_STORAGE__BATCH_WORKERS` | Worker count in `batch` mode; each owns an independent batch. |

`mode = "off"` applies to the **synchronous** endpoint, where the caller already holds the answer. It does not disable persistence for `POST /{channel}/async`: appending `/async` *is* a request for a result to be fetched later, so the trace row is written before the `202` is returned and `trace_id` is always present. `off` is safe to combine with async channels.

## Related

- [Production Checklist](../operate/production-checklist.md) — which of these
  settings to change before an instance takes real traffic.
- [Secure an Instance](../operate/security.md) — the security settings in
  context, with the reasoning.
- [CLI Reference](./cli.md) — `validate-config`, `migrate`, and the rest.
