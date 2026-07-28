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
| 8 | [Review the smaller changes](#7-smaller-behaviour-changes) | Always |

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
There is no attempt cap and no give-up path: the retry loop exits only on a
committable outcome or on shutdown. Because messages are processed
sequentially, this halts **every partition of every subscribed topic** on that
instance, not just the poison message's partition. Restarting does not help —
the offset was never committed, so the same message is redelivered.

Symptoms: consumer lag climbing on all partitions, `orion_errors_total{type="kafka_retry"}`
incrementing on a ~60s cadence, and the same message logged repeatedly.

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
**unauthenticated admin API**. They now default to `ORION_ENV=production` and
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
`ORION_ENV=development`. A keyless non-dev install needs **both**
`adminAuth.enabled=false` **and** `env=development` — with `env: production`
and no keys the pod passes templating and then CrashLoops at config validation.

For HA compose, set the host-side variable (note the name differs from the
container-side `ORION_ADMIN_AUTH__API_KEYS`):

```bash
export ORION_ADMIN_API_KEYS="key-one,key-two"
docker compose -f docker-compose.ha.yml up -d
```

**`ORION_ENV=production` forces exactly two things**, and the second one
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

Nothing else keys off `production` — logging, TLS, and route exposure are
unaffected.

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

## 7. Smaller behaviour changes

### Polling an async trace now requires the token returned with the 202

**What changed.** `POST /api/v1/data/{channel}/async` returns a `trace_token`
alongside `trace_id`, and `GET /api/v1/data/traces/{id}` requires it — via the
`x-trace-token` header or a `?token=` query parameter — unless the caller
presents an admin credential. Previously the endpoint was all-or-nothing admin
auth: open to everyone on a default config (so any caller could read any other
caller's payloads by walking trace ids) and closed to the submitter when admin
auth was on.

The trace *list* (`GET /api/v1/data/traces`) is unchanged in its auth but now
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
curl -s "http://orion:8080/api/v1/data/traces/$id" -H "x-trace-token: $tok"
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
hides them from HTTP responses, and `queue.trace_retention_hours` ages them
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
the persisted trace at `GET /api/v1/data/traces/{id}`. It is also returned as
the `x-request-id` response header. Cached responses store the sanitised body,
so a cache hit is consistent with a miss.

> **This is data-plane only, and it has a gap by default.**
> `GET /api/v1/data/traces/{id}` returns the **unsanitised** result. That
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

**What changed.** `GET /api/v1/data/traces` and `GET /api/v1/data/traces/{id}`
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

**What to do — important.** **Never round-trip a connector config through
`GET` → `PUT`.** `update` replaces `config_json` wholesale rather than merging,
and nothing un-masks on the way in, so a GET-then-PUT writes the literal
`"******"` into the database and permanently destroys the credential. Omit the
`config` field from the `PUT` body to preserve the stored config, or send the
real values. This hazard pre-dates 1.0.0 for keyed fields; the URL rule
broadens it to values that previously round-tripped intact.

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
just loud now instead of swallowed.

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
