# Upgrading to 1.0.0

This page is for operators upgrading an existing Orion deployment from
**0.3.0** (the previous release) to **1.0.0**. It covers only what *breaks* or
*changes behaviour*. New capabilities are in the
[CHANGELOG](https://github.com/GoPlasmatic/Orion/blob/main/CHANGELOG.md); new
configuration keys are in the [Config Reference](../configuration/reference.md).

Every item below is written as **what changed → how you'll notice → what to
do**. Nothing here requires a workflow or channel rewrite; the changes are in
the runtime's request path, deployment defaults, and operational surfaces.

---

## Before you start

Work through this list. Each row links to the section with the detail.

| # | Check | Applies to you if |
|---|-------|-------------------|
| 1 | [Set `rate_limit.trusted_proxies`](#1-rate-limiting-behind-a-proxy-or-load-balancer) | `rate_limit.enabled = true` **and** you run behind a proxy, LB, or ingress |
| 2 | [Update dashboards and alerts](#2-metrics-dashboards-and-alerts-will-break) | You scrape `/metrics` |
| 3 | [Audit stored channel configs](#3-channels-with-broken-stored-config-now-refuse-to-load) | Always — this one can stop the server from booting |
| 4 | [Enable the Kafka DLQ](#4-kafka-delivery-is-now-at-least-once) | `kafka.enabled = true` |
| 5 | [Supply admin API keys](#5-deployment-defaults-helm-and-ha-compose-now-require-admin-keys) | You deploy via the Helm chart or `docker-compose.ha.yml` |
| 6 | [Back up before migrating](#6-database-migrations) | You are on PostgreSQL |
| 7 | [Pass `trace_token` when polling async traces](#polling-an-async-trace-now-requires-the-token-returned-with-the-202) | You submit to `/async` and poll `GET /traces/{id}` without an admin key |
| 8 | [Rename the renamed config keys](#7-config-keys-four-sections-renamed) | You set `[queue]`, `[channels]`, `[tracing.storage]`, or `ORION_ENV` |
| 9 | [Delete `kafka.max_inflight`](#7-config-keys-four-sections-renamed) | You set `kafka.max_inflight` in the config file or `ORION_KAFKA__MAX_INFLIGHT` in the environment — Kafka enabled or not |
| 10 | [Check client URL casing](#rest-routes-match-byte-exactly-and-decode-parameters-once) | You call data-plane REST routes with casing that differs from the channel's `route_pattern` |
| 11 | [Re-check `data_query` / `data_write` envelopes](#the-data-dialect-rejects-what-it-used-to-ignore) | Any workflow uses the portable data dialect |
| 12 | [Re-point anything scraping `/docs`](#docs-and-the-openapi-spec-are-off-in-production) | You fetch `/docs` or `/api/v1/openapi.json` and run with `environment = "production"` |
| 13 | [`chown` existing data volumes](#the-charts-pod-defaults-are-hardened-and-the-images-are-pinned) | You upgrade a Docker or compose deployment with an existing `/app/data` mount |
| 14 | [Review the smaller changes](#8-smaller-behaviour-changes) | Always |

**Take a database backup before upgrading.** Migrations run automatically at
boot unless you set `storage.auto_migrate = false`.

---

## Which backend were you actually on?

This matters more than it sounds, because it decides how much of this page
applies to you.

- **SQLite** — the only storage backend that fully worked in 0.3.0. Assume the
  whole page applies.
- **PostgreSQL** — the 0.3.0 schema migrated cleanly, but the Rust models
  decode `i64` while the migration created `integer` (`INT4`) columns, and
  sqlx-postgres refuses `INT4 → i64`. **Every repository read failed at
  runtime**, so a 0.3.0 Postgres deployment could exist but could not serve.
  You still have a schema to migrate — see
  [Database migrations](#6-database-migrations).
- **MySQL** — the 0.3.0 migration set could not execute through sqlx *at all*
  (mysql-client `DELIMITER` directives, `TEXT` columns with literal defaults,
  `TEXT` primary keys without prefix lengths). No MySQL deployment can exist to
  upgrade. Treat MySQL as new in 1.0.0 and start from an empty database.

---

## 1. Rate limiting behind a proxy or load balancer

**This is the highest-impact change on the page.** It applies only when
`rate_limit.enabled = true` (still `false` by default), but where it applies it
changes who shares a bucket.

**What changed.** The rate limiter's client identity used to be read straight
from `X-Forwarded-For` (first element) or `X-Real-IP`, falling back to the
literal string `"unknown"` when neither was present. Any client could mint a
fresh bucket by sending a made-up header. The identity is now the **TCP peer
address**, and forwarded headers are honoured *only* when the peer address
falls inside the new `rate_limit.trusted_proxies` list. That list is **empty by
default**, which means "trust nothing".

**How you'll notice.** If Orion sits behind a proxy, LB, ingress controller, or
service mesh, the TCP peer is always that hop — so **every client collapses
into a single bucket** and legitimate traffic starts getting `429`s far below
the configured rate. Watch `orion_rate_limit_rejections_total` climb while real
request volume is unchanged.

**What to do.** List the addresses your proxies connect from, as CIDR blocks or
bare IPs (IPv4 and IPv6 both accepted):

```toml
[rate_limit]
enabled = true
trusted_proxies = ["10.0.0.0/8", "192.168.1.1", "fd00::/8"]
```

Or by environment variable (comma-separated; it **replaces** the list, it does
not append):

```bash
ORION_RATE_LIMIT__TRUSTED_PROXIES="10.0.0.0/8,fd00::/8"
```

On Kubernetes this is your pod or node CIDR; behind a cloud LB it is the LB's
subnet, not the client's. Orion canonicalises IPv4-mapped IPv6 peers, so a
server bound on `[::]` still matches an IPv4 CIDR.

Two things to know:

- **A malformed entry is a hard startup failure, even when
  `rate_limit.enabled = false`.** The message is
  `rate_limit.trusted_proxies: invalid entry '<x>': expected an IP address or CIDR block (e.g. "10.0.0.0/8")`.
  Run `orion-server validate-config` before you deploy.
- **Per-channel `rate_limit.key_logic` is affected too.** Any channel whose
  key expression references `{"var": "client_ip"}` now receives the peer
  address under the same rules.

> **Not changed:** sticky rollout bucketing still reads forwarded headers
> directly and does not consult `trusted_proxies`. See
> [Sticky rollouts](#sticky-canary-rollouts-are-now-caller-stable).

---

## 2. Metrics: dashboards and alerts will break

**What changed.** Two label changes, both intended to bound Prometheus
cardinality.

**`orion_rate_limit_rejections_total` lost its `client` label and gained `scope`.**
The metric name is unchanged. The old label carried a raw client IP — unbounded
cardinality. `scope` takes one of a small, bounded set of values:

| `scope` value | Meaning |
|---------------|---------|
| *(channel name)* | A per-channel limiter defined in that channel's `config_json` |
| `admin` | The platform limiter for `/api/v1/admin*` |
| `data` | The platform limiter for `/api/v1/data*` |
| `operational` | Everything else |

**Channel-labelled metrics now use `_unknown` for unregistered channels.** When
a request names a channel that is not in the channel registry, the `channel`
label is the literal string `_unknown` (leading underscore) instead of the
caller-supplied name — otherwise anyone could inflate the metric cardinality by
POSTing to arbitrary paths. Three metrics are capped:

- `orion_messages_total{channel, status}`
- `orion_message_duration_seconds{channel}`
- `orion_channel_executions_total{channel}`

The cap applies to the HTTP and async-queue paths. Kafka ingest is deliberately
exempt — its channel set comes from operator configuration and is already
bounded.

**How you'll notice.** Silently. A PromQL selector on a label that no longer
exists returns an empty result rather than an error, so a `by (client)`
breakdown renders as an empty panel and an alert built on it **stops firing**
instead of erroring.

**What to do.** Grep your dashboards and rules for the old label before you
upgrade:

```bash
grep -rn 'rate_limit_rejections_total' \
  grafana/ dashboards/ prometheus/ *.rules.yml
```

Then rewrite the selectors:

```promql
# before
sum by (client) (rate(rate_limit_rejections_total[5m]))
# after
sum by (scope) (rate(rate_limit_rejections_total[5m]))
```

If you are already running Prometheus, you can confirm which series the old
label produced before cutting over:

```promql
count by (client) (rate_limit_rejections_total)
```

**Four metrics are new** and worth adding to your dashboards while you are in
there:

| Metric | Type | Why you want it |
|--------|------|-----------------|
| `orion_trace_queue_rejected_total{reason}` | counter | `reason="full"` or `"memory"` — async submissions being shed with a `503`. See [Queue-full](#queue-full-now-returns-503-instead-of-hanging) |
| `orion_trace_dlq_depth` | gauge | Backlog of failed traces. **Only refreshed by the DLQ retry loop, so it stops updating when `queue.dlq_retry_enabled = false`** — exactly the setting that lets the backlog grow |
| `orion_trace_dlq_retries_total{outcome}` | counter | `retried` / `exhausted` / `failed` |
| `orion_trace_persistence_failures_total` | counter | The only signal that trace writes are being dropped |

No metrics were renamed or removed.

---

## 3. Channels with broken stored config now refuse to load

**What changed.** When a channel's stored `config_json` failed to parse, or its
`validation_logic` failed to compile, Orion used to log a warning and serve the
channel anyway — **with that channel's validation, dedup, rate limit, cache and
backpressure guards silently disabled**. A channel whose `validation_logic` was
broken was, in effect, an unvalidated channel. That is now a refusal, in both
single-node and cluster mode.

**How you'll notice.** Loudly, and in three separate places:

- **At startup, the process exits with status 1 and never binds a port.** One
  broken active channel takes down the whole server.
- **At reload, the operation fails wholesale.** The registry rebuild is
  all-or-nothing, so the previous engine and registry stay in place and healthy
  channels keep serving — but `POST /api/v1/admin/engine/reload` returns `500`
  with code `CONFIG_ERROR`. So does **every admin mutation that triggers a
  reload**: activate, archive, delete, and rollout updates. The admin plane is
  effectively wedged for status changes until the row is fixed. (Draft creates
  and updates do not reload, so you can still edit your way out.)
- **In cluster mode**, the epoch watcher logs
  `Epoch watcher: resync failed; will retry`, increments
  `orion_errors_total{type="epoch_watcher"}`, and stops advancing — nodes quietly
  stop picking up *any* config change.

Log lines to grep for:

```
Refusing to load channel: config_json does not parse
Refusing to load channel: validation_logic does not compile
```

The aggregate error printed to stderr at boot, and returned in the admin
response body, reads:

```
refused to load N channel(s): <channel>: config_json does not parse: <serde error>; ...
```

**What to do — before you upgrade.** Only rows with `status = 'active'` are
parsed, and only those surviving your `engine.channels.include` / `exclude`
patterns. Start by listing exactly what the server will try to load:

```sql
SELECT channel_id, version, name, config_json
FROM channels
WHERE status = 'active'
ORDER BY name;
```

On PostgreSQL you can sweep for the type mismatches that actually fail. These
are fields with a required type and no default:

```sql
SELECT channel_id, version, name, config_json
FROM channels
WHERE status = 'active'
  AND pg_input_is_valid(config_json, 'jsonb')          -- PG16+; else cast and catch
  AND (
       jsonb_typeof(config_json::jsonb #> '{rate_limit,requests_per_second}') NOT IN ('number','null')
    OR jsonb_typeof(config_json::jsonb #> '{cache,ttl_secs}')                 NOT IN ('number','null')
    OR jsonb_typeof(config_json::jsonb #> '{timeout_ms}')                     NOT IN ('number','null')
    OR jsonb_typeof(config_json::jsonb #> '{backpressure,max_concurrent}')    NOT IN ('number','null')
    -- required whenever the parent object is present:
    OR (config_json::jsonb ? 'backpressure'  AND NOT (config_json::jsonb->'backpressure')  ? 'max_concurrent')
    OR (config_json::jsonb ? 'cache'         AND NOT (config_json::jsonb->'cache')         ? 'enabled')
    OR (config_json::jsonb ? 'deduplication' AND NOT (config_json::jsonb->'deduplication') ? 'header')
    OR (config_json::jsonb ? 'rate_limit'    AND NOT (config_json::jsonb->'rate_limit')    ? 'requests_per_second')
  );
```

On SQLite, `json_valid(config_json) = 0` catches outright malformed JSON:

```sql
SELECT channel_id, version, name FROM channels
WHERE status = 'active' AND json_valid(config_json) = 0;
```

**SQL cannot predict the `validation_logic` case** — that requires actually
compiling the JSONLogic expression. The only complete pre-flight is to **run
the 1.0.0 binary against a restored snapshot or a read replica** and confirm it
boots. That exercises the identical code path and names every offending channel
in one error.

> **Unknown fields are still ignored.** `ChannelConfig` does not use
> `deny_unknown_fields`, so a stray key in `config_json` — including the
> removed [`backpressure.queue_depth`](#backpressurequeue_depth-was-removed) —
> parses fine. Only invalid JSON and wrong *types* on known fields fail.

---

## 4. Kafka delivery is now at-least-once

**What changed.** The consumer used to commit the offset unconditionally,
whatever happened to the message — so a message that failed processing was
simply lost. Offsets now advance only on **successful processing** or a
**confirmed dead-letter write**. Everything else (validation rejection, UTF-8
decode failure, JSON parse failure, unmapped topic, empty payload, timeout,
engine error, workflow errors) leaves the offset uncommitted and retries the
same message in place, with backoff starting at 1s and doubling to a 60s cap.
Each retry cycle increments `orion_errors_total{type="kafka_retry"}`.

**How you'll notice.** With `kafka.dlq.enabled = false` — which is still the
default — a permanently-poison message **stalls the consumer indefinitely**.
There is no give-up path: the message is never dropped and its offset is never
committed. The in-place retrying *is* bounded, but only so that the consumer
keeps polling — one cycle runs for at most **80% of `max.poll.interval.ms`**
(240s against librdkafka's 300s default, or 80% of the value you set for
`max.poll.interval.ms` in `kafka.extra_config`). On expiry the consumer seeks
the partition back to the message's offset and returns to the poll loop, so the
very same message is redelivered and the cycle starts over. The stall is
therefore unchanged from an operator's point of view, but the consumer keeps
polling: it stays in its group instead of being evicted for exceeding
`max.poll.interval.ms`, and rebalance callbacks stay live. Because messages are
processed sequentially, this halts **every partition of every subscribed
topic** on that instance, not just the poison message's partition. Restarting
does not help — the offset was never committed, so the same message is
redelivered.

Symptoms: consumer lag climbing on all partitions, `orion_errors_total{type="kafka_retry"}`
incrementing on a ~60s cadence, `orion_errors_total{type="kafka_retry_budget_exhausted"}`
incrementing once per budget expiry, and the same message logged repeatedly.

**What to do.** Enable the dead-letter queue. This is the recommended action
and turns the stall into an advancing offset plus a message you can inspect:

```toml
[kafka.dlq]
enabled = true
topic = "orion-dlq"   # default
```

```bash
ORION_KAFKA__DLQ__ENABLED=true
ORION_KAFKA__DLQ__TOPIC=orion-dlq
```

The DLQ envelope is
`{"source_topic", "error", "original_payload", "timestamp"}`. Create the topic
ahead of time if your broker has auto-creation disabled — a DLQ write that
fails is *not* a confirmed write, and the message keeps retrying.

If you are already stalled without a DLQ, your options are: enable the DLQ and
restart, fix the workflow or channel so the message processes, or advance the
consumer-group offset externally with
`kafka-consumer-groups --reset-offsets`. Removing the topic → channel mapping
does **not** help; unmapped topics take the same failure path.

> **Do not set `enable.auto.commit` in `kafka.extra_config`.** The passthrough
> is applied last and would override the manual-commit setting this guarantee
> depends on.
>
> **`kafka.dlq.*` is unrelated to `queue.dlq_*`.** The latter is the trace DLQ
> — a database table for failed trace persistence, with its own retry loop.

---

## 5. Deployment defaults: Helm and HA compose now require admin keys

**What changed.** Both shipped deployment paths used to bring up an
**unauthenticated admin API**. They now default to `ORION_ENVIRONMENT=production` and
require admin API keys.

**How you'll notice.**

- **Helm:** `helm install` / `helm upgrade` fails at *template* time, before
  anything reaches the cluster:

  ```
  adminAuth.existingSecret or adminAuth.apiKeys is required: the chart defaults
  to a production install with admin auth enforced. Set devStack.enabled=true
  for a throwaway dev install.
  ```

- **HA compose:** `docker compose up` aborts with
  `set ORION_ADMIN_API_KEYS (comma-separated admin API keys)`.

**What to do.** For Helm, supply keys as a chart-managed Secret:

```bash
helm upgrade --install orion deploy/helm/orion \
  --set-string adminAuth.apiKeys[0]="$ORION_ADMIN_KEY"
```

or point at a Secret you manage, which must expose the key `api-keys`:

```yaml
adminAuth:
  existingSecret: orion-admin-keys
```

Escape hatch: `devStack.enabled=true` skips the check and forces
`ORION_ENVIRONMENT=development`. A keyless non-dev install needs **both**
`adminAuth.enabled=false` **and** `env=development` — with `env: production`
and no keys the pod passes templating and then CrashLoops at config validation.

For HA compose, set the host-side variable (note the name differs from the
container-side `ORION_ADMIN_AUTH__API_KEYS`):

```bash
export ORION_ADMIN_API_KEYS="key-one,key-two"
docker compose -f docker-compose.ha.yml up -d
```

**`ORION_ENVIRONMENT=production` forces three things**, and the second one
surprises people:

1. **Admin auth must be enabled and have at least one key**, or the server
   refuses to boot.
2. **CORS wildcard `*` is rejected.** The *default* `[cors] allowed_origins` is
   `["*"]` — so a config that never mentioned CORS at all, and booted fine
   before, now fails with
   `CORS wildcard '*' is not allowed when environment starts with 'prod'. Set explicit origins in [cors] allowed_origins`.
   Set explicit origins before you flip to production:

   ```toml
   [cors]
   allowed_origins = ["https://app.example.com"]
   ```

3. **`/docs` and `/api/v1/openapi.json` are not served** unless you opt back in
   with `server.docs.enabled = true` — see
   [the section below](#docs-and-the-openapi-spec-are-off-in-production).

Nothing else keys off `production` — logging and TLS are unaffected.

### The chart's pod defaults are hardened, and the images are pinned

**What changed.** The chart shipped with no `securityContext` at all, so it
failed Pod Security Standards `restricted` and every policy scanner out of the
box; it inherited Kubernetes' default `maxUnavailable: 25%`, which at two
replicas removes a pod before its replacement is Ready and defeats the
graceful-drain design; the migrate Job inlined the full
`postgres://user:pass@…` URL into its pod spec when `storage.existingSecret`
was unset; and the compose files floated on `:latest`.

Chart installs now run non-root with a **read-only root filesystem** — the only
writable paths are an emptyDir at `/tmp` and the data volume at `/app/data` —
no capabilities, `allowPrivilegeEscalation: false`, and the `RuntimeDefault`
seccomp profile. Rolling deploys surge instead of dipping
(`maxUnavailable: 0`, `maxSurge: 1`), a soft pod anti-affinity spreads replicas
across nodes, and a `startupProbe` on `/healthz` gives boot a five-minute
budget before liveness (10s period, 3 failures) takes over. The migrate Job
reads the storage URL through `secretKeyRef` in every case.

**How you'll notice.**

- A workload that wrote anywhere else in the container filesystem now fails —
  override `podSecurityContext` / `securityContext` in values if you need it.
  `POST /api/v1/admin/backups` is one such writer: `storage.backup_dir`
  defaults to `./backups`, which is on the now read-only rootfs. Set
  `persistence.enabled=true` (the chart then points `backup_dir` at the data
  volume) or give `storage.backup_dir` a path under a writable mount.
- The cluster needs headroom for one extra replica during an upgrade, or
  override `strategy`. Setting `affinity` replaces the default anti-affinity
  verbatim.
- **Images built from this Dockerfile run as UID:GID `10001:10001`** instead of
  the previously auto-assigned system UID. Bind mounts and named volumes
  created by an older image (`/app/data` under Docker or compose) may need
  `chown -R 10001:10001`; on Kubernetes the chart's new `fsGroup: 10001`
  handles PVC ownership on mount. If you override `podSecurityContext`, carry
  the numeric `runAsUser`/`runAsGroup`/`fsGroup` forward or `runAsNonRoot`
  verification fails against the image's user.
- The migrate Job's hook-scoped Secret copy — `<release>-orion-storage-migrate`,
  rendered only when `storage.existingSecret` is unset — is not release-managed,
  so `helm uninstall` leaves it behind; delete it manually if you want it gone.
  The Secret the server replicas read is a normal release resource and is
  removed as usual.
- `docker-compose.yml`, `docker-compose.ha.yml` and
  `examples/postgres-orders/docker-compose.yml` now pin
  `ghcr.io/goplasmatic/orion:${ORION_VERSION:-1.0.0}` instead of `:latest`. Set
  `ORION_VERSION` to move. Local HA builds moved to an override file that
  retags them `orion:local`, so `docker compose build` can no longer clobber
  the published tag:

  ```bash
  docker compose -f docker-compose.ha.yml -f docker-compose.ha.build.yml up -d --wait
  ```

**Single-node SQLite installs are now first-class.** Set
`persistence.enabled=true` (a PVC at `/app/data`, kept on uninstall) together
with `cluster.enabled=false`, `replicaCount=1`, `strategy.type=Recreate` (a
ReadWriteOnce claim cannot serve a surge replica), `migrateJob.enabled=false`
and `storage.autoMigrate=true`. Backups then land under `/app/data/backups`.

**Local `docker build` and `git_hash`.** `.dockerignore` excludes `.git/`, so a
locally built image reports `git_hash=unknown` from `/health`, `/metrics` and
`--version` unless you pass the SHA (the published images now do):

```bash
docker build --build-arg GIT_HASH=$(git rev-parse --short HEAD) -t orion .
```

---

## 6. Database migrations

Migrations run at boot unless `storage.auto_migrate = false`, in which case run
`orion-server migrate` as a deploy step. **In multi-replica deployments set
`auto_migrate = false`** so replicas do not race each other at boot.

| Backend | New since 0.3.0 | Notes |
|---------|-----------------|-------|
| SQLite | `004_cluster_coordination` | Additive, fast |
| PostgreSQL | `004_bigint_columns`, `005_active_immutability`, `006_cluster_coordination` | See below |
| MySQL | `001` rewritten; `004`, `005` added | No 0.3.0 deployment can exist — start fresh |

**PostgreSQL: `004_bigint_columns` needs care.** It drops the
`current_workflows` and `current_channels` views, widens `integer` columns to
`bigint` on `workflows`, `channels`, and `trace_dlq`, then recreates the views.
The `ALTER … TYPE bigint` rewrites those tables under an `ACCESS EXCLUSIVE`
lock — they hold definition rows and a failed-trace backlog rather than
request volume, so this is normally quick, but it does block all access while
it runs.

The migration is **not idempotent**: the `DROP VIEW` statements have no
`IF EXISTS`. If the connection drops midway, the database is left without its
two views, and sqlx will not re-run version 004 because it is already recorded.
**Take a backup first.** To recover manually, recreate the two views using the
`CREATE VIEW` statements at the bottom of
`migrations/postgres/004_bigint_columns.sql`.

Preview what will run before committing:

```bash
orion-server migrate --dry-run
```

---

## 7. Config keys: four sections renamed

**What changed.** Four sections and one environment variable were renamed, and
audit-log retention moved out of `[queue]` into its own section with its own
cleanup cadence.

| Pre-1.0 | 1.0 |
|---|---|
| `[queue]` | `[trace_queue]` |
| `queue.trace_retention_hours` | `trace_queue.retention_hours` |
| `queue.trace_cleanup_interval_secs` | `trace_queue.cleanup_interval_secs` |
| `queue.audit_retention_days` | `audit.retention_days` |
| *(none)* | `audit.cleanup_interval_secs` — new, default `3600` |
| `[channels]` | `[channel_filter]` |
| `[tracing.storage]` | `[trace_storage]` |
| `ORION_ENV` | `ORION_ENVIRONMENT` |
| `kafka.max_inflight` | *removed* — see below |

Every other `[queue]` key keeps its name under `[trace_queue]`, and every
`[tracing.storage]` key keeps its name under `[trace_storage]`. Environment
variables follow: `ORION_QUEUE__*` → `ORION_TRACE_QUEUE__*`,
`ORION_CHANNELS__*` → `ORION_CHANNEL_FILTER__*`, `ORION_TRACING__STORAGE__*` →
`ORION_TRACE_STORAGE__*`.

**Why.** Each name was wrong in a way that cost a paragraph to explain.
`[queue]` only ever configured the async trace queue. `[channels]` selects
*which* channels an instance loads and configures none of them.
`queue.trace_cleanup_interval_secs` drove the audit cleanup job too, so the
docs had to say so in three places. `[tracing]` is OpenTelemetry export while
`[tracing.storage]` is Orion's own `traces` rows — unrelated concerns nested
under one name. `ORION_ENV` was the only variable not derived from its field
path.

**One key was removed outright: `kafka.max_inflight`** (and
`ORION_KAFKA__MAX_INFLIGHT`). It was configured, validated and logged — and
inert: the consumer acquired a permit and then awaited each message inline, so
concurrency was always exactly 1 whatever the value said. That sequential
behaviour is load-bearing for the
[at-least-once contract](#4-kafka-delivery-is-now-at-least-once) — committing
an offset implicitly commits every earlier offset on the partition, so
in-consumer concurrency would let a fast later message commit past a failed
earlier one and lose it. Rather than ship a knob that lies, 1.0 removes it.
Nothing about runtime behaviour changes; delete the key and the variable. To
increase throughput, run more Orion instances in the same consumer group
(`kafka.group_id`) — Kafka spreads the partitions across them.

**How you'll notice.** Both halves fail loudly:

- **Config file** — a retired key is rejected by `deny_unknown_fields`
  (see the next section) and the error names it.
- **Environment** — a retired `ORION_*` name is a startup error listing every
  offender and its replacement:

  ```
  Error: Configuration error: these environment variables were renamed or
  removed in 1.0 and are no longer read (see
  docs/src/getting-started/upgrading.md):
    ORION_ENV -> ORION_ENVIRONMENT
    ORION_QUEUE__WORKERS -> ORION_TRACE_QUEUE__WORKERS
  ```

  A removed variable names its reason rather than a replacement, so
  `ORION_KAFKA__MAX_INFLIGHT` reports `removed in 1.0 (K4): Kafka messages are
  processed strictly sequentially per consumer …`.

  This is deliberate rather than convenient. Overrides are matched by name, not
  deserialized, so nothing would otherwise notice that `ORION_QUEUE__WORKERS`
  had stopped applying. For `ORION_ENV` specifically, silence would be a
  security regression: falling back to `development` turns the production
  admin-auth and wildcard-CORS checks from startup errors back into warnings.

**What to do.** Rename the keys in your config file and your deployment
manifests, then confirm with:

```bash
orion-server validate-config -c config.toml
```

The Helm chart and `docker-compose.ha.yml` were updated in this release; if you
templated your own manifests from them, `ORION_ENV` is the one to grep for
first.

**One behaviour change beyond the renames:** audit cleanup now runs on
`audit.cleanup_interval_secs` instead of borrowing the trace job's interval. If
you had tuned `queue.trace_cleanup_interval_secs` to control *both* jobs, set
both new keys to that value to preserve the old behaviour.

---

## 8. Smaller behaviour changes

### Every admin response is now wrapped in `data`

**What changed.** Three response envelopes used to coexist on the admin plane:
`{"data": …}`, the paginated `{data, total, limit, offset}`, and — from ten
handlers — the fields bare at the top level. Now there is one. Every admin 2xx
body puts its payload under `data`; list endpoints add the three pagination
counters alongside it and nothing else.

**How you'll notice.** Ten endpoints return a body one level deeper than before:

| Endpoint | Was | Now |
|---|---|---|
| `GET /admin/engine/status` | `{version, uptime_seconds, …}` | `{"data": {…}}` |
| `POST /admin/engine/reload` | `{reloaded, workflows_count}` | `{"data": {…}}` |
| `GET /admin/connectors/circuit-breakers` | `{enabled, breakers}` | `{"data": {…}}` |
| `POST /admin/connectors/circuit-breakers/{key}` | `{reset, key}` | `{"data": {…}}` |
| `POST /admin/trace-dlq/purge` | `{purged, older_than_hours}` | `{"data": {…}}` |
| `POST /admin/workflows/{id}/test` | `{matched, trace, output, errors}` | `{"data": {…}}` |
| `POST /admin/workflows/validate` | `{valid, errors, warnings}` | `{"data": {…}}` |
| `POST /admin/{workflows,channels,connectors}/import` | `{imported, failed, errors}` | `{"data": {…}}` |
| `GET /admin/traces/{id}` | bare trace object | `{"data": {…}}` |

Everything already returning `{"data": …}` — all CRUD reads and writes, every
list endpoint, `GET /admin/functions`, `POST`/`GET /admin/backups`,
`GET /admin/traces` — is byte-identical. Only the ten rows above changed.

**What to do.** Add `.data` to the affected call sites:

```bash
# before
curl -s localhost:8080/api/v1/admin/engine/status | jq '.workflows_count'
# after
curl -s localhost:8080/api/v1/admin/engine/status | jq '.data.workflows_count'
```

Error bodies are unaffected — they stay `{"error": {code, message}}`, so
`.data` present and `.error` present remain mutually exclusive. The data plane
is unaffected too: `POST /api/v1/data/…` still answers
`{"status": "ok", "data": …}` as before.

The `orion-server dry-run` CLI subcommand prints the **unwrapped** shape
(`{matched, trace, output, errors}`) — it writes JSON to stdout for `jq`, not
an HTTP response, so it gains nothing from an envelope.

### Bulk import reports dry runs in the same fields as real runs

**What changed.** `POST /admin/{workflows,channels,connectors}/import?dry_run=true`
used to return six fields for two facts: `would_create` and `would_fail`
alongside a hardcoded `imported: 0` and a `failed` that always equalled
`would_fail`. Both modes now return the same four fields.

```jsonc
// before, ?dry_run=true
{ "dry_run": true, "would_create": 12, "would_fail": 1, "imported": 0, "failed": 1, "errors": [...] }
// after, ?dry_run=true          (and wrapped in `data`, per the section above)
{ "data": { "dry_run": true, "imported": 12, "failed": 1, "errors": [...] } }
// after, real run
{ "data": { "dry_run": false, "imported": 12, "failed": 1, "errors": [...] } }
```

**What to do.** Read `imported`/`failed` in both modes and branch on `dry_run`.
The one trap: in a dry run `imported` is now the count that *would* be created,
where it used to be a constant `0`. Any check of the form `if imported == 0` as
a proxy for "this was a dry run" will now be wrong — test `dry_run` instead.

**Not changed:** all three imports still return **200** even when every item
failed, so check `failed` rather than the status code.

### `data_write` takes its envelope under `write`

**What changed.** The mutation envelope is now nested, mirroring
`data_query`'s `query`:

```jsonc
// before — envelope flat, sharing a namespace with the handler keys
{ "name": "data_write", "input": {
    "connector": "orders_db", "op": "update", "target": "users",
    "set": { "status": "inactive" },
    "filter": { "==": [{ "field": "id" }, { "param": "id" }] },
    "params": { "id": { "var": "data.req.id" } },
    "output": "data.updated" } }

// after — `connector`/`schema`/`params`/`database`/`output` stay at the top
{ "name": "data_write", "input": {
    "connector": "orders_db",
    "params": { "id": { "var": "data.req.id" } },
    "output": "data.updated",
    "write": {
      "op": "update", "target": "users",
      "set": { "status": "inactive" },
      "filter": { "==": [{ "field": "id" }, { "param": "id" }] } } } }
```

**How you'll notice.** You won't — the flat form is still honoured, so existing
workflows keep running. It is documented as deprecated and will be removed in a
later major.

**What to do.** Move the eight envelope keys — `op`, `target`, `values`, `set`,
`filter`, `on_conflict`, `returning`, `all` — into a `write` object, leaving
`connector`, `schema`, `params`, `database` and `output` where they are. If a
task carries both shapes, `write` wins, so a half-finished migration cannot
silently run the stale envelope.

**Why.** The two halves of one dialect read differently, and because the
envelope shared a namespace with the handler it could never grow a field named
`connector`, `schema`, `params`, `database` or `output`. Nesting also means
there is one JSON value that *is* the envelope — validation errors now point at
`…function.input.write.target` instead of a path that could mean either half.

### `response_path` is now called `output`

**What changed.** Eight of the ten connector functions named their destination
path `output`; `http_call` and `channel_call` named it `response_path`. All ten
now take `output`.

**How you'll notice.** You won't — `response_path` is still honoured, so
existing workflows keep running. It is documented as deprecated and will be
removed in a later major. If a task somehow carries both keys, `output` wins.

**What to do.** Rename the key at your leisure:

```json
// before
{ "name": "http_call", "input": { "connector": "crm", "response_path": "data.customer" } }
// after
{ "name": "http_call", "input": { "connector": "crm", "output": "data.customer" } }
```

The *defaults* are unchanged and still differ by function: omitting `output` on
`http_call` discards the response, while every other handler writes to `"data"`.

### The trace read endpoints moved to the admin plane

**What changed.** Both trace endpoints moved:

| Before | After |
|---|---|
| `GET /api/v1/data/traces` | `GET /api/v1/admin/traces` |
| `GET /api/v1/data/traces/{id}` | `GET /api/v1/admin/traces/{id}` |

**There is no redirect.** The old paths now resolve as *channel* names on the
data-plane catch-all, so a request to one returns 404 (or runs a channel you
happen to have named `traces`) rather than a 308.

**Why.** The list endpoint was already admin-guarded, so its placement on the
data plane was a naming lie. It was also a functional one: `/traces` and
`/traces/{id}` were static routes, and axum resolves static segments before the
`/{*path}` catch-all — so **a channel named `traces` was permanently
unreachable**, `POST /api/v1/data/traces` returned 405, and the rate limiter
carried a special case to skip the name. None of that was documented or
checked; it just silently didn't work.

**How you'll notice.** Any async client that polls `GET /api/v1/data/traces/{id}`
starts getting 404. Operator tooling hitting the list gets the same.

**What to do.** Update the paths. The access rules are unchanged: the list needs
an admin credential, and the single-trace GET still takes *either* an admin
credential or the submission's `trace_token` (see the next section) — despite
now living under `/api/v1/admin`, which is the one path in that namespace not
covered by the blanket admin guard.

```bash
# before
curl "http://orion:8080/api/v1/data/traces/$id"  -H "x-trace-token: $tok"
# after
curl "http://orion:8080/api/v1/admin/traces/$id" -H "x-trace-token: $tok"
```

### Polling an async trace now requires the token returned with the 202

**What changed.** `POST /api/v1/data/{channel}/async` returns a `trace_token`
alongside `trace_id`, and `GET /api/v1/admin/traces/{id}` requires it — via the
`x-trace-token` header or a `?token=` query parameter — unless the caller
presents an admin credential. Previously the endpoint was all-or-nothing admin
auth: open to everyone on a default config (so any caller could read any other
caller's payloads by walking trace ids) and closed to the submitter when admin
auth was on.

The trace *list* (`GET /api/v1/admin/traces`) is unchanged in its auth but now
returns payload-free rows — `input_json`, `result_json` and `task_trace_json`
are served only by the single-trace GET, whose `message` also no longer
includes the submitter's request context (`context.metadata`).

**How you'll notice.** A polling client that ignores `trace_token` starts
getting `401` on its next poll.

**What to do.** Capture `trace_token` from the 202 and send it on each poll:

```bash
resp=$(curl -s -X POST http://orion:8080/api/v1/data/orders/async \
  -H 'Content-Type: application/json' -d '{"data":{"order_id":1}}')
id=$(jq -r .trace_id <<<"$resp"); tok=$(jq -r .trace_token <<<"$resp")
curl -s "http://orion:8080/api/v1/admin/traces/$id" -H "x-trace-token: $tok"
```

Operator tooling that already sends an admin key needs no change. Traces
created before the upgrade have no token and stay on the admin trust model.
The migration adding `traces.access_token_hash` runs automatically on all
three backends.

### Credential headers are masked in workflow metadata

**What changed.** `metadata.headers` now carries `"******"` for
`authorization`, `cookie`, `proxy-authorization` and `x-api-key`. Their
plaintext values previously reached `traces.result_json` and
`trace_dlq.metadata_json`.

**How you'll notice.** `validation_logic` that compares a credential header's
*value* stops matching. Testing header *presence* still works.

**What to do.** If a channel used `rollout.sticky_header` pointing at a
credential header, switch it to a non-credential header — otherwise every
caller now hashes into the same rollout bucket. Rows written before the
upgrade still contain plaintext headers at rest; the trace-read projection
hides them from HTTP responses, and `trace_queue.retention_hours` ages them
out.

### Response cache keys changed format

**What changed.** The per-channel response cache key hashed only the request
body. It now also folds in the HTTP method, route parameters, and query string
(both sorted, so ordering does not affect the key). The old key could serve one
caller's cached response to a different request that happened to share a body.

**How you'll notice.** A one-time cache miss spike after the upgrade.

**What to do.** Nothing is required. The key prefix (`cache:{channel}:{hash}`)
is unchanged and carries no version segment, so old entries are **orphaned, not
mis-served** — a new request cannot reproduce an old hash. They expire on their
own via `cache.ttl_secs` (default `300` seconds when unset). There is no
cache-flush endpoint; for a guaranteed-clean cutover on Redis:

```bash
redis-cli --scan --pattern 'cache:*' | xargs -r redis-cli DEL
```

With the in-memory backend, entries are process-local and a restart clears
them.

### Data-plane error bodies are sanitised

**What changed.** Workflow task errors returned on the data plane used to carry
full internal detail — the raw error message, workflow ID, task path, and retry
state. Each entry in `errors[]` is now reduced to a code, a fixed generic
message, and (when present) the task ID:

```json
{
  "id": "...",
  "status": "ok",
  "data": { },
  "errors": [
    {
      "code": "TASK_ERROR",
      "message": "Task processing failed; full detail is available in the trace",
      "task_id": "enrich"
    }
  ],
  "request_id": "0f8c…"
}
```

**How you'll notice.** Any client parsing `errors[*].message` for detail gets
the same constant string every time.

**What to do.** Correlate on `request_id` — note it is a **top-level sibling of
`errors[]`**, not a field inside each entry — and fetch the full detail from
the persisted trace at `GET /api/v1/admin/traces/{id}`. It is also returned as
the `x-request-id` response header. Cached responses store the sanitised body,
so a cache hit is consistent with a miss.

> **This is data-plane only, and it has a gap by default.**
> `GET /api/v1/admin/traces/{id}` returns the **unsanitised** result. That
> endpoint is guarded only when `admin_auth.enabled = true`, and the default is
> `false`. Enable admin auth for the sanitisation to hold end to end.

### Open circuit breakers return 503 `CIRCUIT_OPEN`

**What changed.** A request rejected by an open circuit breaker used to surface
as `500` with code `ENGINE_ERROR`, indistinguishable from a genuine engine
fault. It is now `503` with code `CIRCUIT_OPEN`.

**How you'll notice.** Look at `$.error.code` in the top-level error envelope:

```json
{"error": {"code": "CIRCUIT_OPEN",
           "message": "Circuit breaker open for connector 'orders-db' on channel 'orders'",
           "request_id": "..."}}
```

There is **no `Retry-After` header** on this response.

**What to do.** Update any client or alert that matched on `500` /
`ENGINE_ERROR` for breaker rejections, and treat `503 CIRCUIT_OPEN` as
retryable.

> **Blind spot worth knowing.** This only holds when the failing task has
> `continue_on_error: false` (the default). With `continue_on_error: true` the
> request returns **HTTP 200** with a sanitised `TASK_ERROR` entry, and the
> string `CIRCUIT_OPEN` appears nowhere in the response — the breaker rejection
> is visible only in the persisted trace and in
> `orion_circuit_breaker_rejections_total{connector, channel}`. Alert on the metric,
> not on the status code, if your workflows use `continue_on_error: true`.

### Queue-full now returns 503 instead of hanging

**What changed.** When the async trace queue was full, submission blocked
waiting for capacity — an unbounded hang under load. It now sheds immediately.

**How you'll notice.** `POST /api/v1/data/{channel}/async` (and the REST-routed
`…/async` equivalents) returns **`503`** with code **`SERVICE_UNAVAILABLE`** —
*not* `QUEUE_FULL`. Disambiguate from other 503s by the message
(`Trace queue is full (N messages pending)` or
`Trace queue memory limit exceeded …`) or, better, by
`orion_trace_queue_rejected_total{reason="full"|"memory"}`.

**What to do.** Make async clients retry on `503`. Size the queue with
`queue.buffer_size` (default `1000`) and `queue.max_queue_memory_bytes`
(default `104857600`, 100 MB). Sync requests never touch this queue.

### Trace read endpoints require admin auth

**What changed.** `GET /api/v1/admin/traces` and `GET /api/v1/admin/traces/{id}`
return full input and result payloads but were reachable without a key even
with `admin_auth.enabled = true`. They are now guarded alongside
`/api/v1/admin/*` and `/metrics`.

**How you'll notice.** Previously-open callers polling for async results get
`401`. URLs are unchanged.

**What to do.** Send the admin key on those requests. **No effect if
`admin_auth.enabled = false`** (still the default) — which is also why enabling
admin auth is recommended, see the sanitisation gap above.

### Sticky canary rollouts are now caller-stable

**What changed.** The rollout bucket was `rand::random` per request, so a
caller in a 10% canary flip-flopped between versions call to call and replica
to replica. The bucket is now a stable hash of a caller identity:
`engine.rollout_sticky_header` when configured (e.g. `x-user-id`), otherwise
the forwarded client IP (`X-Forwarded-For` first element, else `X-Real-IP`).
Requests with no identity keep the random fallback, so percentages still hold
in aggregate.

**How you'll notice.** A given caller now consistently gets the same version.
The *population* split still matches the configured percentage, but it is no
longer re-drawn per request — a 10% canary that previously exposed nearly every
caller occasionally now exposes a stable 10% of callers.

**What to do.** Set the identity header explicitly if IP is a poor proxy for
your callers (NAT, mobile, shared egress):

```toml
[engine]
rollout_sticky_header = "x-user-id"
```

```bash
ORION_ENGINE__ROLLOUT_STICKY_HEADER=x-user-id
```

> Unlike rate limiting, this path reads forwarded headers **without**
> consulting `rate_limit.trusted_proxies`, so the identity is caller-influenced.
> That is acceptable for canary assignment and is not a security control — do
> not use rollout percentages to gate access.

### Unimplemented secret schemes are rejected

**What changed.** `vault://`, `aws-sm://`, `gcp-sm://`, and `azure-kv://` in
connector configs were never resolved — the reference string was passed through
and **used as the literal password**. Those four schemes are now rejected at
connector load. `env://` still works, and ordinary URLs (`postgres://`,
`redis://`, `https://`) are untouched.

**How you'll notice.** The connector is **skipped at load** with an `ERROR` log
— the server still boots, and `POST`/`PUT` of such a connector through the
admin API still returns `201`/`200`. The connector is simply absent, and
workflows referencing it fail at request time. Grep for:

```
Failed to resolve secret reference in connector config, skipping
```

whose `error=` field reads
`connector '<name>' config_json: secret scheme 'vault://' is reserved but not supported in this build; supply the value via env:// or a literal instead`.

**What to do.** Find them before upgrading — matching is case-sensitive and
lowercase-only, as in the code:

```sql
SELECT id, name, connector_type, enabled
FROM connectors
WHERE config_json LIKE '%vault://%'
   OR config_json LIKE '%aws-sm://%'
   OR config_json LIKE '%gcp-sm://%'
   OR config_json LIKE '%azure-kv://%';
```

Replace each with `env://VAR_NAME` and inject the secret through your
orchestrator. If such a connector appeared to work before, it was authenticating
with the literal string `vault://...` as its password — rotate that credential.

### Connector reads redact credentials inside URLs

**What changed.** `GET /api/v1/admin/connectors` already masked
secret-named keys (`password`, `token`, `api_key`, …) with `******`. It now
also strips **userinfo from URL-shaped values at any depth**, so
`https://elastic:hunter2@es:9200` comes back as `https://elastic:******@es:9200`.
This is what finally covers `url` and `brokers[]`. A credential-free URL is
still shown in full — masking it wholesale would hide connector endpoints from
the admin UI for no security gain.

**Query parameters with secret-looking names are masked too.** `?api_key=…`,
`?sig=…` and `?X-Amz-Signature=…` used to round-trip in the clear inside a URL
value. The parameter name is now judged by the same predicate as an object key,
and that predicate gained `bearer`, `dsn` and `webhook` (substring matches)
plus `pat` and `sig` (exact matches).

**What to do — nothing, but know the round-trip rules.** `update` replaces
`config_json` wholesale rather than merging, so a `GET` → edit → `PUT` sends
masked values back. Each masked position — a masked field, the userinfo
password, each secret-named query value — is restored from the stored row
*independently*, so rotating one in-URL secret while returning the other still
masked does the right thing. A mask with no stored counterpart is refused with
`400` naming the field rather than silently overwriting a credential, and so is
a literal `******` sent under a non-secret query parameter name (masking can
never produce one there). Omit the `config` field from the `PUT` body entirely
if you do not intend to change it.

**One credential shape is still shown in the clear:** a token embedded in a URL
*path* — a Slack-style webhook — under a generic key such as `url`, because a
path segment carries no name to judge. Store it under a secret-looking key
(`webhook_url`) and the key-name rule masks the whole value.

### Audit-log queries reject unknown parameters

**What changed.** `GET /api/v1/admin/audit-logs` used to ignore unrecognised
query parameters, so a typo silently returned **unfiltered** results that
looked like a successful narrow query. Unknown parameters now return `400`.

**How you'll notice.**

```json
{"error": {"code": "BAD_REQUEST",
           "message": "Invalid query string: Failed to deserialize query string: unknown field `resource_types`, expected one of `offset`, `limit`, `action`, `resource_type`, `resource_id`, `principal`, `start_time`, `end_time`"}}
```

**What to do.** The accepted parameters are exactly those eight. `limit`
defaults to 50 and is clamped to `[1, 1000]`; `offset` defaults to 0;
`start_time` is inclusive and `end_time` exclusive, both accepting RFC 3339,
`%Y-%m-%dT%H:%M:%S`, or `%Y-%m-%d %H:%M:%S`. This strictness applies to this
endpoint only — no other route changed.

### `db_read` returns values for float and blob columns

**What changed.** `float4` / `REAL` / `FLOAT` columns and blob columns silently
returned `null`. They now return values, and a column that genuinely cannot be
decoded raises an error instead of nulling. A `null` in the result now means
only "SQL NULL".

- `Real` → JSON number.
- `Blob` (`bytea`, SQLite `BLOB`, MySQL `TEXT`/`JSON`) → JSON **string**: the
  UTF-8 text when the bytes are valid UTF-8, otherwise **lowercase hex** with
  no `0x` prefix (not base64).

New errors, all prefixed `db_read:`:

```
db_read: column 'x' is unreadable: <sqlx error>
db_read: column 'x' (Real) failed to decode: <sqlx error>
db_read: column 'x' holds NaN, which JSON cannot represent
```

**How you'll notice.** Workflows that used a `null` check to skip a float or
blob column now see real data; a query touching an undecodable column now fails
where it previously returned a row of nulls.

**What to do.** Review JSONLogic that treats these columns as always-null.
**Scope note:** this affects `db_read`, `data_query` (including nested
`include` queries), and `data_write`'s `RETURNING` path. It does **not** affect
`db_write`, which returns `{"rows_affected", "last_insert_id"}`.

> Postgres `timestamptz`, `uuid`, `jsonb`, `numeric`, arrays, and enums were
> **never** silently nulled — sqlx rejects them while building the row, so the
> query already failed loudly. That behaviour is unchanged.

### `backpressure.queue_depth` was removed

**What changed.** The field was parsed but never read. Backpressure rejects
immediately at `max_concurrent` via `try_acquire`; there is no wait queue, so
the field promised behaviour that never existed.

**What to do — nothing, and that is the point.** `ChannelConfig` does not use
`deny_unknown_fields`, so a stored `config_json` still carrying `queue_depth`
**parses and loads normally**; the key is ignored. It does *not* interact with
the [new load strictness](#3-channels-with-broken-stored-config-now-refuse-to-load).
Remove it at your leisure. To find them:

```sql
SELECT channel_id, version, name FROM channels
WHERE config_json LIKE '%queue_depth%';
```

The one case that does fail is `backpressure` present with `queue_depth` but
**no `max_concurrent`** — that field is required and always was; the failure is
just loud now instead of swallowed. (It is named
[`max_concurrent_per_node`](#backpressuremax_concurrent-is-now-max_concurrent_per_node)
as of this release, with `max_concurrent` accepted as a deserialization alias
for one release, so a stored config satisfies the requirement under either
spelling.)

### Storage pool defaults: a docs correction, not a behaviour change

**No runtime default changed between 0.3.0 and 1.0.0.** The 0.3.0 *documentation*
disagreed with the code — the config reference said `max_connections = 25` and
`config.toml.example` said `10`, while the code has always defaulted to `50`.
The docs are now generated from the code.

**What to do.** If you sized your database against the documented number, check
it against the real one. The actual defaults are:

| Key | Default |
|-----|---------|
| `storage.max_connections` | `50` |
| `storage.min_connections` | `5` |
| `storage.acquire_timeout_secs` | `3` |
| `storage.idle_timeout_secs` | `300` |
| `storage.busy_timeout_ms` | `5000` (SQLite only) |

In cluster mode this multiplies: *replicas × `max_connections`* must fit inside
your PostgreSQL `max_connections`, minus headroom for the migration job and
your own tooling.

### The data dialect rejects what it used to ignore

**What changed.** `data_query`/`data_write` no longer approximate silently.
Five changes can turn a previously "working" workflow into an explicit error —
in every case the old behaviour was silently returning wrong or incomplete
data.

- **Unknown envelope keys are rejected.** Stray or misspelled keys in the
  `query` envelope, the `write` envelope, an `include` selection, `on_conflict`
  or the inline `schema` (at any level) now fail. They used to be ignored:
  `"fileds"` selected every column, `"lmit": 5000` fell back to the default
  100, and a misspelled `filter` key made a delete unfiltered. *How you'll
  notice:* a task fails with `unknown key '…' in query envelope` (or `write
  envelope`, `include.<relation>`, `on_conflict`, or an unknown-field error
  from the schema). *What to do:* fix the key — the error names it. The pre-1.0
  flat `data_write` form is still accepted.
- **If you copied the old schema example, it never did what you thought.**
  `"table": "app_users"` was silently dropped — no rename, and identity mode
  where you believed you had an allowlist. The field is `physical`, and
  `"type": "string"` should be `"text"`. The strict schema surfaces this as an
  error instead of silently under-protecting.
- **`include` and many-to-many filters error on MongoDB and Elasticsearch.**
  They used to return parents with silently empty children, or wrong rows; both
  now raise `FeatureUnsupportedByTarget`. *What to do:* on a doc store, fetch
  the related documents with a second query, or model them embedded/nested and
  filter with `some`.
- **Mongo projections no longer include `_id` unless you project it.**
  `fields: ["name"]` now returns `{name}` on every backend. Project the id
  explicitly if you relied on it.
- **`skip` is capped at `query.max_skip` (default `10000`) on every backend.**
  A deeper offset is rejected, never clamped — SQL and MongoDB previously
  accepted any depth. Raise `query.max_skip` (or `ORION_QUERY__MAX_SKIP`) if
  you genuinely page deeper.

### `/docs` and the OpenAPI spec are off in production

**What changed.** Swagger UI (`/docs`) and `/api/v1/openapi.json` used to be
registered unconditionally and unauthenticated, so every deployment published
the complete admin API surface to anonymous callers. They are now gated by
`server.docs.enabled`: unset (the default) serves them only when `environment`
does not start with `prod`; an explicit `true`/`false` always wins.

**How you'll notice.** With `environment = "production"`, `GET /docs` and
`GET /api/v1/openapi.json` return **404** — not 401. The routes are not
registered at all, so their existence is not advertised either.

**What to do.** Usually nothing; this is the intended hardening. If production
tooling reads the served spec, either set `server.docs.enabled = true` (or
`ORION_SERVER__DOCS__ENABLED=true`) to opt back in, or switch to
`orion-server dump-openapi > spec.json`, which works offline regardless of this
setting.

### Workflow caches, dedup stores and response caches no longer share one keyspace

**What changed.** Every `backend: "memory"` cache connector, plus the built-in
dedup store and response cache, shared a single in-process instance and one LRU
budget. In-memory backends are now separate instances per purpose (workflow
cache / dedup / response cache) and per connector name, each with its own
`engine.max_memory_cache_entries` budget.

**How you'll notice.** Only if something depended on the aliasing: a workflow
`cache_read` can no longer observe dedup or response-cache entries (or another
memory connector's keys), and a workflow `cache_write` can no longer influence
dedup or response-cache decisions. Memory contents never survived a restart, so
there is no data migration.

**What to do.** Re-check your sizing if the host is memory-constrained:
`engine.max_memory_cache_entries` is now a **per-namespace** bound, so the
worst case is that value × (2 built-in stores + up to 3 namespaces per memory
connector). Divide the setting by your namespace count to keep the old ceiling.

Redis cache connectors are deliberately *not* partitioned: pointing a workflow
connector and a channel's dedup store at the same Redis database still shares a
keyspace — use separate databases (`redis://host/0`, `/1`, …) where you need
isolation.

### Non-http schemes are refused by SSRF validation

**What changed.** The SSRF validator accepted any URL scheme and only checked
the resolved addresses; it now rejects anything outside `http`/`https` before
any DNS work.

**How you'll notice.** An `http_call` or Elasticsearch egress whose URL uses
another scheme (`gopher://`, `ftp://`, `file://`, …) fails with *"only http and
https are allowed"*. No supported configuration produced such URLs, so this
should be invisible.

### REST routes match byte-exactly and decode parameters once

**What changed.** Three visible changes on `/api/v1/data/*`:

- **Case matters now.** `/ORDERS/1` no longer matches a channel declaring
  `/orders/{id}` — it 404s. Fix client URLs, or register the alternate casing
  as its own route (two casings are two distinct routes now, so both can be
  active).
- **`metadata.params` arrive percent-decoded exactly once.** `/orders/a%2Fb`
  now matches with `id == "a/b"`; previously `%2F` was decoded *before*
  matching and acted as a path separator, so the request never matched at all.
  If a workflow hand-decoded a param, remove that step — decoding twice changes
  meaning (`a%252Fb` arrives as `a%2Fb`, not `a/b`).
- **Malformed escapes are refused.** A path carrying an invalid
  percent-sequence (`%ZZ`, a truncated `%2`) is answered with `400` instead of
  being matched literally.

Percent-encoding an unreserved character is still equivalence per RFC 3986:
`/%6Frders/1` matches `/orders/{id}`.

**Also a validation-time break:** a `route_pattern` containing `%` is now
rejected on create, update and import. Patterns are written literally and
requests match by their decoded value, so an escape in a pattern was only ever
reachable through a double-encoded request — write the literal character
instead. Already-active channels keep their existing behaviour until you next
edit them.

### `backpressure.max_concurrent` is now `max_concurrent_per_node`

**What changed.** The limit was always per node (N replicas admit up to N× the
value); the name now says so, which matters because dedup and rate limiting sit
in the same config block and *are* cluster-shared.

**What to do.** Nothing immediately — stored configs using `max_concurrent`
keep working, as it is a deserialization alias for this release. Update the key
the next time you edit the channel; the alias is scheduled for removal in the
following release.

### Async submissions are exempt from trace sampling

**What changed.** Channels with `trace_storage.sample_rate < 1.0` serving
`/async` traffic used to write the trace's status rows but drop its *result* —
a `completed` trace with nothing in it, and the storage was spent anyway. The
result is now always persisted for an async submission: the 202's `trace_id` is
a receipt for a fetchable result, exactly as `mode = "off"` is already upgraded
to `sync` on that path.

**What to do.** If you used `sample_rate` to bound async trace storage, switch
to `errors_only = true` or tighten `trace_queue.retention_hours`. The sync path
samples exactly as configured, and a sampled-out sync trace now leaves no row
at all.

### Optional: fail closed when a guard's backend is down

`rate_limit.on_backend_error` and `deduplication.on_backend_error` are new
per-channel settings accepting `"allow"` (the default, today's fail-open
behaviour) or `"deny"`, which refuses requests with `503` while the guard's
backend cannot answer — never a `409` or `429`, because the key or limit is
unverifiable rather than violated. Nothing changes unless you set it. Consider
`"deny"` on payment or idempotency-critical channels, where a Redis blip
silently removing all idempotency is worse than refusing the request.

### Duplicate creates now return 409

`POST /api/v1/admin/workflows` and `POST /api/v1/admin/channels` with an id
that already exists now return `409 Conflict` with
`{"error": {"code": "CONFLICT", "message": "…"}}`. Through 0.3.x these returned
`500 INTERNAL_ERROR`. Clients or retry logic that treated the 500 as transient
should treat the 409 as a permanent client error — pick a different id, or use
the import endpoints, which report conflicts per item without failing the
batch.

### Workflow export reads in bounded pages

`GET /api/v1/admin/workflows/export` still returns every matching workflow in
one response; it now reads the database in bounded 500-row pages instead of one
unbounded query.

**One caveat if you use export as a backup:** it is no longer a point-in-time
snapshot. The pages are independent queries, so a workflow created, deleted or
renamed during an export can be missed or appear twice in a single response.
Quiesce workflow mutations during export, or re-export until two consecutive
responses match, when you need a consistent copy.

If you embed Orion as a library, note that `WorkflowRepository::list` now
honours its filter's `limit`/`offset` (default 50, max 1000 per call) instead
of returning the whole table — page through it if you need everything.

### `validate-config` prints the full effective config

**What changed.** `orion-server validate-config` no longer prints the old
hand-maintained summary of a dozen settings. By default it prints the *full
effective config* — every section, merged from defaults, the config file and
`ORION_*` overrides — as TOML on stdout, with secrets masked (`******` for
key-named secrets, passwords struck out of URL-shaped values such as
`storage.url`). Under `--format toml` and `--format json` the
`Configuration is valid.` note goes to stderr so stdout stays machine-parseable;
`--format summary` keeps it on stdout.

**How you'll notice.** Deploy scripts that grep the old summary (`:8080`-style
host:port lines, `storage: sqlite:orion.db`) stop matching. Anything that read
a database password out of the old output stops working — that was a credential
leak, and it is masked in every format now.

**What to do.** Parse stdout as TOML, or run `--format json` and parse JSON;
`--format summary` restores a short human-readable summary (also masked). Exit
codes are unchanged, so plain pre-flight checks (`validate-config || exit 1`)
need no change.

### `/readyz` and `/health` observe Kafka ingestion

**What changed.** With `kafka.enabled = true`, both probes gain a
`components.kafka` field, and **`/readyz` returns 503 while ingestion is
degraded** — that is, a consumer (re)start failed and the built-in restart
supervisor (new in 1.0, capped 1s → 60s backoff) has not yet brought one back.
Previously a node in that state reported ready while consuming nothing.
`/health` reports `status: "degraded"` while HTTP itself keeps serving.

**What to do.** If your readiness alerting assumed only the database, engine or
startup could unready a node, account for the new component; a degraded node
now leaves the load-balancer rotation. The `orion_kafka_ingest_degraded` gauge
(0/1) carries the same signal for Prometheus. Deployments with Kafka disabled
see byte-identical probe bodies.

### New operational settings, no action required

- **`storage.backup_retention_count`** — unset by default, which keeps every
  backup (the pre-1.0 behaviour). Set it to bound SQLite backups: after each
  successful `POST /api/v1/admin/backups` the oldest `orion_backup_*.db` files
  are pruned so at most N remain. `0` is refused at startup. Env override
  `ORION_STORAGE__BACKUP_RETENTION_COUNT`; set it to an empty string to clear.
- **`orion_job_last_success_timestamp_seconds{job}`** — a gauge for the
  background jobs (`trace_cleanup`, `audit_cleanup`, `dlq_retry`,
  `epoch_watcher`, `kafka_lag`). Alert on
  `time() - orion_job_last_success_timestamp_seconds{job="…"}` exceeding a few
  tick intervals: the jobs swallow per-tick errors by design, so this gauge
  going stale is the only signal that cleanup or DLQ retry has silently
  stalled.

---

## Recommended after upgrading

None of these are required, but each closes a gap 1.0.0 opened up for you.

**Hash your admin API keys at rest.** Plaintext keys in `admin_auth.api_keys`
still work unchanged. You can now store a SHA-256 digest instead, and clients
keep presenting the plaintext key:

```bash
printf '%s' "$ORION_ADMIN_KEY" | shasum -a 256 | awk '{print "sha256:"$1}'   # macOS
printf '%s' "$ORION_ADMIN_KEY" | sha256sum   | awk '{print "sha256:"$1}'     # Linux
```

```toml
[admin_auth]
enabled = true
api_keys = ["sha256:9f86d081884c7d659a2feaa0c55ad015a3bf4f1b2b0b822cd15d6c15b0f00a08"]
```

The digest is a plain SHA-256 of the raw key bytes — no salt, no iteration.
Use `printf '%s'`, not `echo`, so you do not hash a trailing newline. A malformed
entry fails config validation with
`admin_auth.api_keys: 'sha256:' entries must be followed by the 64-character hex SHA-256 digest of the key`.

Two consequences to plan for:

- **Audit `principal` values change shape for hashed keys.** A plaintext key
  logs the first 8 characters of the presented token plus `...`
  (`my-secre...`); a hashed entry logs `sha256:` plus the first 8 hex
  characters of the digest plus `...` (`sha256:1a2b3c4d...`). The
  `?principal=` audit filter is an exact match, so saved queries against the
  old prefix stop matching.
- **A plaintext key whose literal text starts with `sha256:`** is now
  interpreted as the hash-at-rest form and will fail config validation. Rotate
  it first.

**Enable admin auth** if you have not. It is what guards `/metrics`, the trace
endpoints, and therefore the full-detail error payloads.

**Enable the Kafka DLQ** — see [section 4](#4-kafka-delivery-is-now-at-least-once).

**Set `storage.auto_migrate = false`** in any multi-replica deployment and run
`orion-server migrate` as a deploy step.

---

## One fix worth retrying: TLS

**TLS was unusable in 0.3.0.** Setting `server.tls.enabled = true` panicked the
process at boot with:

```
Could not automatically determine the process-level CryptoProvider
```

rustls 0.23 auto-selects a cryptography backend only when exactly one is
enabled in the dependency graph, and Orion's graph enables both —
`axum-server` and `reqwest` pull `rustls/aws-lc-rs`, while `mongodb` and `sqlx`
pull `rustls/ring`. The server installs the `aws-lc-rs` provider explicitly
before loading certificates as of 1.0.0, and the path now has test coverage.

If you tried HTTPS, hit that panic, and terminated TLS at a proxy instead: it
works now.

```toml
[server.tls]
enabled = true
cert_path = "/etc/orion/tls/server.crt"
key_path  = "/etc/orion/tls/server.key"
```

---

## Getting help

- [Config Reference](../configuration/reference.md) — every key, with defaults
- [Observability](../features/observability.md) — the full metrics list
- [Maintainability](../features/maintainability.md) — backup, restore, and audit logs
- [CHANGELOG](https://github.com/GoPlasmatic/Orion/blob/main/CHANGELOG.md)
- [Open an issue](https://github.com/GoPlasmatic/Orion/issues)
