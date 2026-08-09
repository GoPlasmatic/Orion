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

**Run this first.** It answers the database-backed rows below — 3, 7b, 14, and
the two channel-config renames — against your actual estate rather than in the
abstract, and exits non-zero if it finds anything:

```bash
orion-server preflight
```

It is read-only, needs only `storage.url`, and reports each finding with the
checklist row it belongs to. Run it with the **1.0 binary against your 0.3.0
database**, before you start the rollout. Config-file and `ORION_*` problems are
reported separately by `orion-server validate-config`.

Then work through this list. Each row links to the section with the detail.

| # | Check | Applies to you if |
|---|-------|-------------------|
| 1 | [Set `rate_limit.trusted_proxies`](#1-rate-limiting-behind-a-proxy-or-load-balancer) | You run behind a proxy, LB, or ingress — **whether or not** `rate_limit.enabled` is set, if any channel declares a `rate_limit` block |
| 2 | [Update dashboards and alerts](#2-metrics-dashboards-and-alerts-will-break) | You scrape `/metrics` — three families changed name or labels, and `/metrics` now 404s when metrics are off |
| 3 | [Audit stored channel configs](#3-channels-with-broken-stored-config-now-refuse-to-load) | Always — this one can stop the server from booting |
| 3b | [Remove unknown keys from channel configs](#unknown-keys-in-a-channel-config-are-now-refused) | Any stored channel config carries a key Orion does not recognise — including the pre-1.0 `cors` and `backpressure.max_concurrent` spellings, `queue_depth`, and typos that were silently ignored before |
| 4 | [Enable the Kafka DLQ](#4-kafka-delivery-is-now-at-least-once) | `kafka.enabled = true` |
| 5 | [Size every ingress against the channel's guards](#every-ingress-applies-the-channels-rate-limit-dedup-and-backpressure) | Any channel declaring `rate_limit`, `deduplication`, `backpressure` or `timeout_ms` is reached over Kafka, `/async`, or `channel_call` — **this one silently throttles or suppresses live traffic** |
| 6 | [Supply admin API keys](#5-deployment-defaults-helm-and-ha-compose-now-require-admin-keys) | You deploy via the Helm chart or `docker-compose.ha.yml` |
| 7 | [Back up before migrating](#6-database-migrations) | You are on PostgreSQL |
| 7b | [Re-point anything reading `workflows.tags` or `channels.methods`](#two-json-columns-were-renamed) | You query Orion's tables directly — dashboards, ETL, reporting views, hand-maintained restores |
| 8 | [Stop migrating at boot in a production cluster](#a-production-cluster-may-not-migrate-at-boot) | `environment` starts `prod` **and** `cluster.enabled = true` **and** `storage.auto_migrate = true` — refused at startup now |
| 9 | [Pass `trace_token` when polling async traces](#polling-an-async-trace-now-requires-the-token-returned-with-the-202) | You submit to `/async` and poll `GET /traces/{id}` without an admin key |
| 10 | [Rename the renamed config keys](#7-config-keys-four-sections-renamed) | You set `[queue]`, `[channels]`, `[tracing.storage]`, or `ORION_ENV` |
| 11 | [Delete `kafka.max_inflight`](#7-config-keys-four-sections-renamed) | You set `kafka.max_inflight` in the config file or `ORION_KAFKA__MAX_INFLIGHT` in the environment — Kafka enabled or not |
| 12 | [Audit your `ORION_*` environment](#misspelled-environment-overrides-now-stop-the-boot) | You set any `ORION_*` variable containing `__` that is not on the config reference page |
| 13 | [Check client URL casing](#rest-routes-match-byte-exactly-and-decode-parameters-once) | You call data-plane REST routes with casing that differs from the channel's `route_pattern` |
| 14 | [Declare a `schema` on every `data_query` / `data_write`](#the-data-dialect-rejects-what-it-used-to-ignore) | Any workflow uses the portable data dialect — **this one breaks every 0.x dialect task at its first request**; `orion-server preflight` lists them |
| 14b | [Move the `data_write` envelope under `write`](#data_write-takes-its-envelope-under-write) | Any workflow uses `data_write` — the pre-1.0 flat form is refused, and `preflight` lists the stored tasks still using it |
| 15 | [Stop reading `total` from the trace list](#the-trace-list-no-longer-returns-total-by-default) | You page `GET /api/v1/admin/traces` |
| 16 | [Re-point anything scraping `/docs`](#docs-and-the-openapi-spec-are-off-in-production) | You fetch `/docs` or `/api/v1/openapi.json` and run with `environment = "production"` |
| 17 | [`chown` existing data volumes](#the-charts-pod-defaults-are-hardened-and-the-images-are-pinned) | You upgrade a Docker or compose deployment with an existing `/app/data` mount |
| 17b | [Set `allow_private_urls` on private db/cache/kafka connectors](#connectors-on-private-networks-need-allow_private_urls) | Any `db`, `cache` or `kafka` connector points at a private address — **which is the normal case** |
| 18 | [Review the smaller changes](#8-smaller-behaviour-changes) | Always |

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

> **It is no longer gated on `rate_limit.enabled`.** A channel's own
> `rate_limit` block is enforced on every ingress by the channel guards, keyed
> on the same trusted-proxy-gated client identity, whether or not the platform
> limiter is running — and the audit trail's `details.client_ip` and the
> failed-auth backoff read it too. If you deliberately left
> `[rate_limit] enabled = false` and rely on per-channel limits, you still need
> `trusted_proxies` set; otherwise every client behind the proxy keys on the
> proxy's own address and the whole fleet shares one bucket.

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
POSTing to arbitrary paths. Two metrics are capped:

- `orion_messages_total{channel, status}`
- `orion_message_duration_seconds{channel}`

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
| `orion_trace_dlq_depth` | gauge | Backlog of failed traces. **Only refreshed by the DLQ retry loop, so it stops updating when `trace_queue.dlq_retry_enabled = false`** — exactly the setting that lets the backlog grow |
| `orion_trace_dlq_retries_total{outcome}` | counter | `retried` / `exhausted` / `failed` |
| `orion_trace_persistence_failures_total` | counter | The only signal that trace writes are being dropped |

**Three metric families changed name or labels.** Rewrite these selectors
before upgrading. The failure mode is the same silent one described above: a
PromQL selector on a name or label that no longer exists returns an empty
result rather than an error, so a panel renders blank and an alert built on it
**stops firing** instead of erroring.

| Before | After |
|---|---|
| `orion_channel_executions_total{channel}` | *removed* — use `sum by (channel) (orion_messages_total)` |
| `orion_errors_total{type="…"}` | `orion_errors_total{reason="…"}` |
| `kafka_consumer_lag{topic, partition}` | `orion_kafka_consumer_lag_messages{topic, partition}` |

```promql
# before
sum by (channel) (rate(orion_channel_executions_total[5m]))
# after — a superset, not an identity: the removed counter had two call sites,
# both on the HTTP path, and never saw the Kafka ingest or DLQ paths
sum by (channel) (rate(orion_messages_total[5m]))
# ...or, for what it actually counted:
sum by (channel) (rate(orion_messages_total{status="ok"}[5m]))

# before
sum by (type) (rate(orion_errors_total[5m]))
# after — the label *values* are unchanged, only the key moved
sum by (reason) (rate(orion_errors_total[5m]))

# before
max by (topic, partition) (kafka_consumer_lag)
# after
max by (topic, partition) (orion_kafka_consumer_lag_messages)
```

Find them — note `kafka_consumer_lag` is a substring of its own replacement, so
check each hit rather than blanket-replacing:

```bash
grep -rn 'channel_executions_total\|errors_total{type\|by (type)\|kafka_consumer_lag' \
  grafana/ dashboards/ prometheus/ *.rules.yml
```

**`/metrics` is no longer registered when `metrics.enabled = false`.** It used
to answer `200` with an empty body rendered from an orphan recorder, so a
deployment with metrics off looked like a working scrape target that simply
never had any series. It now returns `404` with the standard error envelope —
including when `admin_auth.enabled = true`, where an unregistered path falls
through to the 404 fallback rather than answering `401`. If a scrape job goes
red on upgrade, that is the misconfiguration becoming visible. Set
`metrics.enabled = true`, or point the job at the new `metrics.bind_addr`
listener described below.

**Two new audit metrics** worth adding while you are here:
`orion_audit_events_dropped_total{reason}` — alert on the counter existing at
all, not on a threshold, because any non-zero value is a hole in the audit
trail — and `orion_audit_queue_depth`. See
[Audit log](#audit-log-new-actor-format-new-fields-two-new-settings).

### Give Prometheus its own listener (optional, recommended)

`/metrics` is guarded by `admin_auth` along with the rest of the admin plane,
so until now every scraper had to hold an admin API key — a credential that can
also rewrite workflows and read trace payloads. `metrics.bind_addr` moves the
endpoint onto its own listener and removes it from the main one:

```toml
[metrics]
enabled = true
bind_addr = "127.0.0.1:9090"    # or a pod IP, or a private Compose network
```

That listener is plain HTTP (`server.tls` governs the main listener only) and
has **no authentication** — the address is the access control, so bind it
somewhere only your scrapers can reach. Startup logs a warning if it is not a
loopback address, refuses to start if it overlaps `server.host`/`server.port`,
and binds it before the main server, so a clash or a permission problem is a
startup failure rather than a silently missing scrape target. It requires
`metrics.enabled = true`; set alone it warns and raises no listener.

It is a move, not a copy: once `bind_addr` is set the main listener returns
`404` for `/metrics`. Update the scrape config in the same change. The listener
joins the same graceful-shutdown path as the main one, so the last scrape of a
node being drained still succeeds.

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
  `orion_errors_total{reason="epoch_watcher"}`, and stops advancing — nodes quietly
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
parsed, and only those surviving your `channel_filter.include` / `exclude`
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
    OR jsonb_typeof(config_json::jsonb #> '{backpressure,max_concurrent_per_node}') NOT IN ('number','null')
    -- required whenever the parent object is present:
    OR (config_json::jsonb ? 'backpressure'  AND NOT (config_json::jsonb->'backpressure')  ? 'max_concurrent_per_node')
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
compiling the JSONLogic expression. `orion-server preflight` runs the real
parser over every stored row and names each offending channel in one report,
which is what the queries above approximate. Running the 1.0.0 binary against a
restored snapshot or a read replica and confirming it boots exercises the same
code path end to end.

> **Unknown fields fail too.** `ChannelConfig` is `deny_unknown_fields` as of
> 1.0, so a stray key in `config_json` — a typo, the removed
> [`backpressure.queue_depth`](#backpressurequeue_depth-was-removed), or either
> of the two renamed keys — fails the same way a wrong type does. See [Unknown
> keys in a channel config are now
> refused](#unknown-keys-in-a-channel-config-are-now-refused). The type sweeps
> above therefore under-report; `preflight` does not.

---

## 4. Kafka delivery is now at-least-once

**What changed.** The consumer used to commit the offset unconditionally,
whatever happened to the message — so a message that failed processing was
simply lost. Offsets now advance only on **successful processing** or a
**confirmed dead-letter write**. Everything else (validation rejection, UTF-8
decode failure, JSON parse failure, unmapped topic, empty payload, timeout,
engine error, workflow errors) leaves the offset uncommitted and retries the
same message in place, with backoff starting at 1s and doubling to a 60s cap.
Each retry cycle increments `orion_errors_total{reason="kafka_retry"}`.

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

Symptoms: consumer lag climbing on all partitions, `orion_errors_total{reason="kafka_retry"}`
incrementing on a ~60s cadence, `orion_errors_total{reason="kafka_retry_budget_exhausted"}`
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
> **`kafka.dlq.*` is unrelated to `trace_queue.dlq_*`.** The latter is the trace DLQ
> — a database table for failed trace persistence, with its own retry loop.

### Every ingress applies the channel's rate limit, dedup and backpressure

**What changed.** The Kafka ingress applied only `validation_logic`. It now
runs the same guard set as HTTP: `rate_limit`, `deduplication`, `backpressure`
and `timeout_ms` as well. `channel_call` and `/async` were similarly partial
and are now complete too. Audit any active channel that declares one of these
blocks **and** is reached by any ingress other than synchronous HTTP —
a Kafka topic, a `/async` submission, or a `channel_call` target:

```sql
SELECT name, config_json FROM current_channels
WHERE status = 'active'
  AND (config_json LIKE '%rate_limit%'
    OR config_json LIKE '%deduplication%'
    OR config_json LIKE '%backpressure%'
    OR config_json LIKE '%timeout_ms%');
```

**How you'll notice.**

- **Rate limit and backpressure throttle the topic.** A record refused because
  the channel is over its limit or at capacity is **not** dead-lettered: the
  offset is left uncommitted and the consumer retries in place with its
  existing capped backoff, then rewinds the partition when the retry budget
  expires. That backoff is the throttle. Expect consumer lag rather than
  errors, and watch `orion_errors_total{reason="kafka_guard_deferred"}` — a
  sustained rate means the topic is being throttled, not that records are being
  lost. Size `requests_per_second` / `max_concurrent_per_node` against the
  topic's real throughput before upgrading.
- **Deduplication suppresses records.** The idempotency key is the record
  header named by `deduplication.header`; if the record carries no such header,
  the **record key** is used. Record keys are usually partition keys, so if
  yours is an *entity* id (a customer, an account) rather than an *event* id,
  every record after the first inside `window_secs` is suppressed and counted
  as `orion_messages_total{status="duplicate"}`. Either set the header on the
  producer, or drop `deduplication` from channels fed by such a topic. A record
  identified as a duplicate is skipped and its offset committed — nothing is
  dead-lettered, because nothing failed. A redelivery of an offset that was
  never committed is recognised as the *same* delivery and runs, so
  at-least-once is intact.
- **`timeout_ms` is clamped, not adopted.** Kafka caps the channel value at
  `kafka.processing_timeout_ms` and `/async` at
  `trace_queue.processing_timeout_ms`. Those two settings are ceilings, not
  defaults: a Kafka dispatch blocks the consumer's poll loop and an `/async`
  dispatch occupies one of a fixed number of queue workers. A channel may
  shorten its deadline anywhere and lengthen it only where nothing shared
  depends on it. Previously these ingresses ignored `timeout_ms` entirely, so a
  channel with a short one and slow background work will now time out where it
  used to complete — raise the channel value if the HTTP deadline was only ever
  meant to bound the synchronous path, and raise the *transport* setting rather
  than the channel's if you need longer there.
- **`channel_call` spends the target channel's rate-limit budget** (bucket key:
  the calling channel, unless `key_logic` says otherwise), so a fan-out that
  calls one channel N times per request needs headroom for N. A refused call
  now surfaces as `429` or `503` instead of `500 ENGINE_ERROR`; clients
  matching on `ENGINE_ERROR` for these conditions need updating. Deduplication
  is deliberately **not** applied to `channel_call` — it would inherit the
  originating request's idempotency key and reject the second call of a
  legitimate fan-out.

**What to do.** Nothing is required. If a Kafka channel carries a `rate_limit`
intended as an HTTP-only control, either remove it or give it a `key_logic`
that distinguishes the ingress: the default bucket key is the topic on Kafka
and the client IP over HTTP, so the same limit is a per-caller rate on each
ingress rather than one shared cap.

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
| SQLite | `004`–`009` (cluster coordination, trace access token, single-draft-on-update, DLQ/audit indexes, trace pagination indexes, JSON column suffixes) | Additive; `008` is the slow one — see below |
| PostgreSQL | `004`–`013` (bigint columns, active immutability, cluster coordination, trace access token, recreated current views, DLQ/audit indexes, `010`–`012` for the trace pagination indexes, and `013` for the JSON column suffixes) | See below |
| MySQL | `001` rewritten; `004`–`012` added | No 0.3.0 deployment can exist — start fresh |

> **Migration numbers are per-backend and are not comparable.** Each backend
> has its own migration directory and its own version sequence, so the same
> number means different things: `004` is `cluster_coordination` on SQLite,
> `bigint_columns` on PostgreSQL and `active_immutability` on MySQL. There is
> no shared version space, and a number alone never identifies a change.
>
> **Refer to a migration by name.** `orion-server migrate --dry-run` prints the
> backend, the number and the name together, which is the unambiguous form:
>
> ```text
> Pending migrations on postgres (2):
>   postgres 012 — drop trace created at index
>   postgres 013 — json column suffixes
> ```

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

**All backends: the trace pagination indexes are the slow part.** They add
`idx_traces_updated_at` and `idx_traces_created_at_id`, then drop
`idx_traces_created_at` — the composite is a strict superset for every query
that used it, including the retention delete's `created_at < cutoff`. On a
`traces` table with millions of rows these two `CREATE INDEX` statements
dominate the whole 1.0 migration. Run it in a maintenance window if your
`traces` table is large, or trim it first with `trace_queue.retention_hours`.

> **PostgreSQL: `010`–`012` run outside a transaction, on purpose.** Each file
> begins with a `-- no-transaction` marker and works `CONCURRENTLY` — `010`
> and `011` `CREATE INDEX CONCURRENTLY`, `012` `DROP INDEX CONCURRENTLY` the
> index they supersede — so they do **not** lock `traces` against writes
> while the indexes build — a plain `CREATE INDEX` holds a `SHARE` lock for the
> whole build, which on a large trace table is a write outage. Two consequences:
>
> - They are **three separate migration versions**, not one, because
>   `CONCURRENTLY` also refuses the implicit transaction PostgreSQL wraps a
>   multi-statement simple query in. `orion-server migrate --dry-run` lists all
>   three; a failure part-way leaves the earlier ones applied and recorded,
>   which is correct — just re-run.
> - A `CONCURRENTLY` build that dies (connection drop, cancellation) leaves an
>   **`INVALID` index** behind: unused by the planner, still maintained on every
>   write. The migrations are `IF NOT EXISTS`, so a re-run skips an invalid
>   leftover rather than repairing it. Check for one before re-running:
>
>   ```sql
>   SELECT c.relname FROM pg_class c JOIN pg_index i ON i.indexrelid = c.oid
>   WHERE NOT i.indisvalid AND c.relname LIKE 'idx_traces%';
>   ```
>
>   Clear it with `REINDEX INDEX CONCURRENTLY <name>;` (PostgreSQL 12+) or
>   `DROP INDEX CONCURRENTLY <name>;` and re-run the migration.
>
> **MySQL** states `ALGORITHM=INPLACE LOCK=NONE`, so it fails loudly rather
> than locking the table if the engine cannot build the index online — but its
> DDL is not transactional, and it has no `CREATE INDEX IF NOT EXISTS`, so a
> part-way failure needs whichever of the two new indexes exists dropped before
> the re-run. **SQLite** has no online build and needs none: it is single-node
> and the migration runs before the listener binds.

#### Two JSON columns were renamed

`workflows.tags` is now `workflows.tags_json`, and `channels.methods` is now
`channels.methods_json`. On MySQL, `traces.access_token_hash` narrows from
`TEXT` to `char(64)`.

**You won't notice through the API.** `tags` and `methods` are still the field
names every workflow and channel endpoint accepts and returns, and the OpenAPI
document is unchanged. You will notice if anything reads Orion's tables
directly: a Grafana panel over `workflows`, an ETL job, a reporting view, a
restore into a schema you maintain by hand. Those fail with "column does not
exist" the first time they run after the upgrade. Query the new names.

| Backend | Migration | What it does |
|---------|-----------|--------------|
| SQLite | `009_json_column_suffixes` | Two `ALTER TABLE … RENAME COLUMN`. SQLite rewrites dependent triggers and views itself. |
| PostgreSQL | `013_json_column_suffixes` | Drops and recreates `current_workflows` / `current_channels`, renames the two columns, replaces both `enforce_*_active_immutable()` bodies. |
| MySQL | `011_json_column_suffixes` | The same shape, plus dropping and recreating the two `trg_*_active_immutable` triggers. |
| MySQL | `012_narrow_access_token_hash` | `traces.access_token_hash` → `char(64)`. |

The extra work on PostgreSQL and MySQL is not defensive. Both store view target
lists and trigger bodies as resolved text, and a bare rename leaves them broken
**without failing the migration**: PostgreSQL's `current_workflows` would keep
publishing a column called `tags` while the table underneath has `tags_json`,
and its immutability trigger would start raising
`record "old" has no field "tags"` on every update of an active row; MySQL's
views would stop resolving at all and its triggers would fail with
`Unknown column 'tags' in 'OLD'`.

**How long it locks.** On PostgreSQL a column rename is a catalog update — it
does not rewrite the table, so the cost does not scale with row count; what can
hurt is *acquiring* the `ACCESS EXCLUSIVE` lock behind a long-running
transaction on `workflows` or `channels`. The whole file runs in one
transaction, so it lands whole or not at all. On MySQL the rename is
metadata-only, but `TEXT` → `char(64)` requires `ALGORITHM=COPY`: a full
rebuild of `traces` with writes blocked for the duration. On a 1.0.0 install
`traces` is empty and this is immediate; against a large trace backlog, size
the window or trim it first with `trace_queue.retention_hours`.

**If it fails.** Take a backup first — the same advice this section already
gives for `004_bigint_columns`. On PostgreSQL, DDL is transactional and the
file carries no `-- no-transaction` marker, so a failed run leaves the schema
untouched and no ledger row: fix what blocked it and start again. On MySQL,
every DDL statement commits implicitly, so an interrupted `011` can leave the
columns renamed with the views and triggers not yet recreated and nothing
recorded; put the two columns back and let it re-run from the top —

```sql
ALTER TABLE `workflows` RENAME COLUMN `tags_json` TO `tags`;
ALTER TABLE `channels`  RENAME COLUMN `methods_json` TO `methods`;
```

— every other statement in the file is `IF EXISTS`-guarded or a `CREATE` after
a matching `DROP`, so once the columns are back the migration is re-runnable.
`012` is a no-op on re-run. On SQLite, restore the file from your backup.

#### A production cluster may not migrate at boot

This is now enforced, not advised. With `environment` starting `prod`,
`cluster.enabled = true` together with `storage.auto_migrate = true` is a
config error and the server refuses to start:

```
Error: Configuration error: cluster.enabled = true with storage.auto_migrate = true
in production: every replica would migrate at boot and race the others. Set
storage.auto_migrate = false (ORION_STORAGE__AUTO_MIGRATE=false) and run
`orion-server migrate` as a deploy step …
```

It is raised during config validation, before anything opens a connection, so
`orion-server validate-config` reports it before a rollout. Previously this
pairing only warned — from a log line emitted *after* the migration it warns
about, so the guardrail fired after the race it existed to prevent.

Set `storage.auto_migrate = false` and run `orion-server migrate` as a deploy
step. The Helm chart already ships a pre-install/pre-upgrade Job
(`migrateJob.enabled`, on by default) and `docker-compose.ha.yml` a one-shot
`migrate` service, so both reference topologies are already in the safe shape
and need no change. With `auto_migrate = false`, a replica whose schema is
behind refuses to start rather than serving against a schema it does not
understand.

**Unaffected:** single-node installs (cluster mode is off by default, and
migrating at boot is what makes the single binary self-installing), and
non-production clusters, which keep the warning. That exemption is what lets
the chart's `devStack` demo run cluster mode without a migrate Job, since its
database is created by the same release.

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

### Misspelled environment overrides now stop the boot

A misspelled override used to be ignored in silence — overrides are matched by
name rather than deserialized, so `ORION_SERVER__PORTT=3000` did exactly
nothing and you found out from a port number in a log line. It is now a startup
error naming every offender at once and suggesting the nearest real key:

```
Error: Configuration error: these ORION_* environment variables are not Orion
settings and would be silently ignored:
  ORION_SERVER__PORTT (did you mean ORION_SERVER__PORT?)
```

**What is affected is narrow.** Only names carrying the `__` section separator
are checked, plus near-misses of `ORION_ENVIRONMENT` (the one setting whose
path has a single segment). A name without a `__` is not a setting name and is
left alone, because `ORION_` is not Orion's to claim. So this does **not**
affect:

- Kubernetes service links. A namespace with a Service called `orion` gives
  every pod `ORION_SERVICE_HOST`, `ORION_PORT`, `ORION_PORT_8080_TCP_ADDR` and
  more unless the PodSpec sets `enableServiceLinks: false`. The chart now sets
  it, but you do not need it: nothing in that block can be refused.
- `orion-cli`'s `ORION_SERVER_URL` / `ORION_API_KEY`, even exported in the
  shell you start the server from.
- Compose interpolation such as `${ORION_VERSION}`, which `docker compose`
  resolves in your shell — it never reaches the server.

Before upgrading, list the `__`-carrying `ORION_*` names in your deployment
manifests — a Deployment's `env:`/`envFrom:`, a Compose `environment:` block, a
systemd unit, the shell you launch the binary from — and check each against
`docs/src/configuration/reference.md`:

```bash
env | grep -oE '^ORION_[A-Z0-9_]+' | grep '__'
```

Two escape hatches for names Orion should not interpret:

- Reference them from your config file with `${VAR}` — substitution reads them
  on Orion's behalf, so they are allowed.
- Or put them under `ORION_SECRET_*`, which is never read as configuration.
  This is the namespace for `env://` connector secrets and for `${VAR}` inside
  a connector `config_json`, since connectors live in the database and cannot
  be enumerated while the config loads. Only a name that
  *could* be a misspelled override needs moving — that is, one carrying the
  `SECTION__KEY` separator. A connector holding
  `"token": "env://ORION_DB__PASSWORD"` needs that variable renamed to
  `ORION_SECRET_DB_PASSWORD` (or out of the prefix entirely) and its `env://`
  reference updated to match; a single-underscore name like
  `ORION_API_TOKEN` is left alone and needs no change.

Everything is reported in one pass, so a single restart confirms a whole
manifest.

One caveat worth knowing: a setting typed with a *single* underscore
(`ORION_SERVER_PORT` for `ORION_SERVER__PORT`) is byte-for-byte the shape of a
service link, so it is ignored rather than reported. Type the double
underscore.

**If you copied `ORION_ADMIN_AUTH__API_KEY` from the deployability page,** the
correct name is `ORION_ADMIN_AUTH__API_KEYS` (plural, comma-separated). The
singular form was never read, so admin auth was enabled with no keys loaded.
The page is fixed, and the singular name is now a startup error rather than a
silent one.

---

### Connectors on private networks need `allow_private_urls`

**What changed.** SSRF protection used to cover the `http` connector and the
Elasticsearch helper, and nothing else — no `db`, `cache`, `mongo` or `kafka`
path checked its endpoint at all. A connector holding
`postgres://…@169.254.169.254/…` was accepted and dialled. Now every connector
type is checked twice: a **scheme allow-list** when it is created or updated,
and a **private-address check** when the connection is first opened.

**How you'll notice.** Two different ways, and only the first is loud:

- On create/update, a connector whose scheme cannot belong to its backend is
  refused with `400` and a message naming the allowed schemes. `db` accepts
  `postgres`, `postgresql`, `mysql`, `mariadb`, `sqlite`, `mongodb`,
  `mongodb+srv`; `cache` (redis) accepts `redis`, `rediss`; `es` accepts
  `http`, `https`; Kafka `brokers` must be bare `host:port`, not URLs.
  **Existing stored connectors are not re-validated** — you meet this the next
  time you edit one.
- At runtime, the **first request** through a connector pointed at a private
  address fails. The response is generic (the data plane is anonymous), but the
  trace carries the full message, naming the address and the flag.

**What to do.** Set `allow_private_urls: true` on every `db`, `cache` and
`kafka` connector whose target is intentionally on a private network — which is
the normal case for a database or a cache:

```json
{
  "connector_type": "db",
  "config": {
    "type": "db",
    "connection_string": "postgres://orion:…@postgres.internal:5432/orion",
    "allow_private_urls": true
  }
}
```

The flag is not a workaround; it is the point. A database on `10.x` is
expected, and stating it keeps the *unstated* case — a workflow-authored
connector reaching `169.254.169.254` — refused by default. Nothing here is
skipped for `sqlite:` connection strings or `backend: "memory"` caches, because
neither opens a socket.

Because the driver re-resolves the hostname when it dials, this is a guard
rather than a guarantee. Keep network-level egress policy where the difference
matters.

---

## 8. Smaller behaviour changes

### Every admin response is now wrapped in `data`

**What changed.** Three response envelopes used to coexist on the admin plane:
`{"data": …}`, the paginated `{data, total, limit, offset}`, and — from ten
handlers — the fields bare at the top level. Now there is one. Every admin 2xx
body puts its payload under `data`; list endpoints add the three pagination
counters alongside it and nothing else — bar the trace list, whose deviation is
described immediately below.

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
list endpoint, `GET /admin/functions`, `POST`/`GET /admin/backups` — is
byte-identical. Only the ten rows above changed *shape*.

**One exception: `GET /admin/traces`.** Its envelope is unchanged, but its
*fields* are not: `total` is now conditional and `next_cursor` is new. See
[the next section](#the-trace-list-no-longer-returns-total-by-default).

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

### The trace list no longer returns `total` by default

**What changed.** `GET /api/v1/admin/traces` used to answer
`{data, total, limit, offset}` on every page. `total` is now **omitted** unless
the request asks for it with `?include_total=true`. Two fields are new:
`next_cursor` on the response, and `cursor` on the request. This is the one
list endpoint that deviates from the shared pagination contract; every other
one still returns `total` unconditionally.

**Why.** `total` was a `COUNT(*)` over the whole filtered set — a full scan on
PostgreSQL and InnoDB — recomputed on *every* page of the largest table Orion
writes to. Most callers page through the list and never read the number. Deep
`offset` paging has the same shape of problem: the database counts past every
skipped row. Keyset (`cursor`) paging skips nothing and counts past nothing, so
page 500 costs what page 1 costs.

**How you'll notice.** Anything doing `.total` on the trace list gets `null`
(jq) or a missing-key error (typed clients). Nothing errors, and nothing else
moved: `data`, `limit` and `offset` are exactly where they were.

**What to do.** Add `include_total=true` where you genuinely need the count,
or — better for anything that walks the list — switch to the cursor:

```bash
# before
curl -s "http://orion:8080/api/v1/admin/traces?limit=100" | jq '.total'

# after: ask for the count explicitly
curl -s "http://orion:8080/api/v1/admin/traces?limit=100&include_total=true" | jq '.total'

# after: page without a count and without an OFFSET
page=$(curl -s "http://orion:8080/api/v1/admin/traces?limit=100")
cursor=$(jq -r '.next_cursor // empty' <<<"$page")
curl -s "http://orion:8080/api/v1/admin/traces?limit=100&cursor=$cursor"
```

`next_cursor` is present only while a further page may exist — its absence is
how you know you have reached the end. Treat the value as **opaque**; its
encoding is not part of the API contract.

**Three request combinations are now `400` rather than silently wrong:**

| Request | Why |
|---|---|
| `?cursor=…&offset=10` | Two different paging modes; pass one |
| `?cursor=…&sort_by=updated_at` (or `status`, `channel`, `mode`) | `updated_at` is rewritten in place by every status change, so a cursor over it would skip rows. Keyset paging is offered only for the default `created_at` ordering |
| a `cursor` value you did not get from a `next_cursor` | Malformed cursor |

`?offset=` still works exactly as before for every sort column, including
`updated_at`; nothing forces you onto the cursor.

If you embed Orion as a library, `TraceRepository::list_paginated` now returns
`TracePage` (with `total: Option<i64>` and `next_cursor`) rather than
`PaginatedResult<Trace>`. The other six repositories are unchanged.

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

**Added in 1.0 (K2), additive:** both modes also carry `unchanged`, `skipped`
and a per-item `results` array, populated by the new `?on_conflict=skip` /
`?on_conflict=new_version` upsert modes — see
[Admin API › Export & Promotion](../api/admin.md#promoting-over-an-existing-estate-on_conflict).
The default `on_conflict=fail` behaves exactly as before.

Also additive in 1.0: a real (non-dry-run) import writes one audit row per
entity written alongside the `"{n} imported"` summary row (K5); channels and
connectors gained `tags` + `?tag=` filtering, matching workflows (K6); status
and rollout changes accept `?reload=defer` to batch engine rebuilds (K4); and
an `X-Orion-Change-Context` request header is recorded in audit `details`
(K5). The 1.0 API also adds `GET /workflows/{id}/dependencies` (K9),
`content_hash` on every entity response (K10), exports that read as one
consistent snapshot (K12), and the `orion-server package` CLI that composes
all of it — see [Admin API › Export & Promotion](../api/admin.md#export--promotion).

### Channel names must be unique

**What changed.** A channel name may belong to only one `channel_id` (K7).
Creating, updating, or importing a channel whose name another channel's
current version already holds answers **409**, and activation refuses a name
another *active* channel holds. Before 1.0 the collision stored cleanly and
was resolved silently at runtime: the data plane and `channel_call` address
channels by **name**, so one of the two won the registry slot and the other's
requests ran the winner's workflow.

**What to do.** Run `orion-server preflight` before upgrading — it reports
every name held by more than one `channel_id` (`channel-names` check). Rename
all but one (new version with a distinct name, activate it) or delete the
redundant channels. An estate without duplicates — any estate that worked
predictably — is unaffected.

### Channel activation now requires an active workflow

**What changed.** `PATCH /admin/channels/{id}/status` with `{"status":
"active"}` answers **400** when the channel's `workflow_id` is unset, names a
workflow that does not exist, or names one with no active version (K8). It
used to succeed and quarantine the channel at the next engine load — the same
outcome, discovered later, with no error to the caller. The docs and the
`/validate` warning always claimed this gate existed; now it does.

**What to do.** Activate in dependency order — connectors → workflows →
channels — which is what any working deployment script already did, since an
out-of-order channel never served. A script that relied on activate-then-fix
ordering must activate the workflow first. `?dry_run=true` on the same
endpoint pre-flights the gate without writing.

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

**How you'll notice.** The flat form is **not** accepted. `write` is a required
input, so a task still in the old shape is refused at create, update, bulk
import, `POST /admin/workflows/validate` and `orion-server lint`, with an error
naming `write`. A workflow already stored in the flat shape fails at its first
request.

Find them before you upgrade:

```bash
orion-server preflight
```

**What to do.** Move the eight envelope keys — `op`, `target`, `values`, `set`,
`filter`, `on_conflict`, `returning`, `all` — into a `write` object, leaving
`connector`, `schema`, `params`, `database` and `output` where they are. Stale
flat keys left behind by a half-finished migration are inert — `write` is the
only envelope — so you can move them one workflow at a time.

**Why.** The two halves of one dialect read differently, and because the
envelope shared a namespace with the handler it could never grow a field named
`connector`, `schema`, `params`, `database` or `output`. Nesting also means
there is one JSON value that *is* the envelope — validation errors now point at
`…function.input.write.target` instead of a path that could mean either half.

### `response_path` is now called `output`

**What changed.** Eight of the ten connector functions named their destination
path `output`; `http_call` and `channel_call` named it `response_path`. All ten
now take `output`.

**How you'll notice.** You won't — `response_path` is still accepted, so
existing workflows keep running. Unlike the other 1.0 renames it carries no
removal date: on `http_call` the alias belongs to the `HttpCallConfig` struct in
`dataflow-rs`, which Orion does not own and cannot remove on its own. It is
listed under [accepted alternate
spellings](../reference/support.md#accepted-alternate-spellings) rather than as
a deprecation. Supplying both keys is a duplicate-field error, not a precedence
rule.

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

### A closed trace queue answers 503, not 500

**What changed.** `TraceQueue::submit` had two adjacent failure arms for one
condition — the queue cannot take this message. Queue *full* answered `503
SERVICE_UNAVAILABLE`; queue *closed* answered `500 QUEUE_ERROR`, which
`is_retryable()` simultaneously reported as retryable. A retryable 500 is a
contradiction, and the OpenAPI document had never described it: it lists
queue-full and queue-closed together under `503`.

Both now answer `503` with code `SERVICE_UNAVAILABLE`. The `QUEUE_ERROR` code is
gone.

**How you'll notice.** Only during shutdown, which is the one time the queue is
closed while requests still arrive. A client retrying on 503 now retries this
too, which is the correct behaviour and was already what the documentation
promised.

**What to do.** Nothing, unless you match on the literal string `QUEUE_ERROR`.

### `BAD_REQUEST` is now `VALIDATION_ERROR`, and an oversized result is a 500

**What changed.** Two 400 codes existed for one condition. Validators mixed
`BAD_REQUEST` and `VALIDATION_ERROR` freely — which one a given refusal
answered with was an accident of which internal variant the code path
constructed, not a distinction a client could rely on. They are merged: every
400 that answered `{"code": "BAD_REQUEST"}` now answers
`{"code": "VALIDATION_ERROR"}`. The message is unchanged, the status is
unchanged, and `details[]` appears on a few more of them (connector
create/update refusals now name the offending field the way channel and
workflow refusals already did).

Separately, `RESPONSE_TOO_LARGE` — a workflow result exceeding
`trace_queue.max_result_size_bytes` — moves from `502 Bad Gateway` to
`500 Internal Server Error`. No upstream is involved in that condition, so 502
was the wrong claim; the code string is unchanged.

And the same one-condition-two-codes merge on the 504: an engine timeout
answered `{"code": "TIMEOUT_ERROR"}` while the channel-level timeout guard
answered `{"code": "TIMEOUT"}` — which one a caller saw was an accident of
which layer fired first. Every 504 now answers `TIMEOUT`. The status and
message are unchanged.

**How you'll notice.** A client branching on `error.code == "BAD_REQUEST"`
stops matching; branch on `VALIDATION_ERROR` (or on the 400 status). Anything
alerting on 502s from the data plane should alert on the `RESPONSE_TOO_LARGE`
code instead. Retry/backoff or paging rules matching `TIMEOUT_ERROR` on 504s
silently stop firing; match `TIMEOUT` (or the 504 status).

**What to do.** Update literal matches on `BAD_REQUEST` and `TIMEOUT_ERROR`.
If you branch only on HTTP status, the 400s and 504s are untouched and the
oversized-result case moves from 502 to 500.

### Updating an entity with no draft is a 404, not a 400

**What changed.** `PUT /api/v1/admin/workflows/{id}` (and the channel
equivalent, and `PATCH …/status` activation) answered `400` with *"No draft
version found"* when the entity had no draft — while every other missing-row
lookup in the admin API answers `404`. Which status a missing thing produced
depended on which lifecycle method you reached first. All no-draft misses now
answer `404 NOT_FOUND` with the same message.

Alongside it, the admin list surfaces were normalised: connector listings
accept `sort_by` (`name` default, `connector_type`, `created_at`,
`updated_at`) and `sort_order` — previously they were hard-wired to
`name ASC`, which remains the default — and the version-history, trace, DLQ
and audit-log query parameters are now declared in the OpenAPI document
instead of only in prose.

**How you'll notice.** Automation that treated a 400 from an update as "no
draft — create one first" sees a 404 for that case now. The 400 still exists
for genuinely invalid input.

**What to do.** Branch on 404 for the no-draft case. If you branched on the
message text, it is unchanged.

### `engine.reload_timeout_secs` and `orion_engine_lock_wait_seconds` are gone

**What changed.** The live engine was held behind a read-write lock, so every
request acquired a read guard and a reload waited for a write guard. It is
published with an atomic store now, so readers never block and a reload never
waits.

Two things existed only to describe that wait and have been removed:

- **`engine.reload_timeout_secs`** (`ORION_ENGINE__RELOAD_TIMEOUT_SECS`) — how
  long a reload would wait for the write lock. There is no wait to bound.
- **`orion_engine_lock_wait_seconds`** — the histogram of that wait. It could
  now only ever report zero.

The `_orion.profile` debug output loses its `engine_lock_wait` phase and
`engine_lock_wait_ms` field for the same reason. `engine.health_check_timeout_secs`
stays — it still bounds the `/readyz` cluster-Redis ping.

**How you'll notice.** Setting `ORION_ENGINE__RELOAD_TIMEOUT_SECS` now **stops
the boot** with a message naming it as removed, rather than being silently
ignored. A `reload_timeout_secs` line in a config file is rejected by
`deny_unknown_fields` the same way.

**What to do.** Delete the setting from any config file, Helm values or
environment. Drop `orion_engine_lock_wait_seconds` from dashboards and alerts —
a panel on it will read empty rather than break.

### `trace_storage.batch_size` now defaults to `1000`

**What changed.** Only the default; `trace_storage.mode` still defaults to
`sync`, and a deployment that has not opted into `batch` or `async` is not
affected by any of this.

For deployments that *have*, a flush costs a fixed per-transaction price plus a
per-row one, so the old default of `100` rows per flush spent most of each
transaction on overhead. Measured on SQLite with 4 workers, the same load
drained at 26k rows/s at `100` and 45k rows/s at `1000` — a tenth as many
transactions for the same rows.

**How you'll notice.** `batch` and `async` modes keep up with a higher request
rate before `max_pending` overruns, and `orion_trace_persistence_batch_size`
reports larger flushes. Trace visibility is unchanged — a partial batch still
flushes on `batch_flush_interval_ms`.

**What to do.** Nothing. Set `batch_size` explicitly to pin the old value:

```toml
[trace_storage]
batch_size = 100
```

### Trace loss under `batch` / `async` now warns in the log

**What changed.** When the persistence queue overruns `max_pending`, the dropped
traces were reported only to `orion_trace_dropped_total{reason="overflow"}` —
and `metrics.enabled` defaults to `false`, so the out-of-the-box signal for
"your traces are being discarded" was a counter nobody was collecting. The drop
now also logs a `WARN`: immediately when the loss starts, then at most once
every 5 seconds, each line carrying how many traces were dropped since the
previous one.

**How you'll notice.** A log line naming the overrun, if you run `batch` or
`async` at a request rate the DB cannot absorb. `sync` cannot produce it.

**What to do.** Treat the line as real data loss, not noise. Raise
`trace_storage.max_pending` / `batch_size`, set `async_on_overflow = "block"` to
slow producers instead of shedding, sample deliberately with `sample_rate` /
`errors_only`, or move to `mode = "sync"` and let the request path be throttled
by the trace table rather than outrun it.

### Response cache keys changed format

**What changed.** Three things, all of which change the hash:

1. The key hashed only the request body. It now also folds in the HTTP method,
   route parameters, and query string (both sorted, so ordering does not affect
   the key). The old key could serve one caller's cached response to a different
   request that happened to share a body.
2. The digest is **SHA-256 truncated to 128 bits**, not FNV-1a. FNV-1a is a
   multiply-xor over 64 bits with no collision resistance — a colliding payload
   is constructed rather than searched for, and the data plane is unauthenticated
   by design, so on most deployments the body is attacker-shaped input. Two
   requests that hash alike are served each other's response bodies.
3. `cache_key_fields` entries now resolve as **paths**, not just literal
   top-level keys. `user.id` walks into a nested object, and `data.user_id` —
   the spelling this guide and the feature docs have always shown — resolves to
   the payload's `user_id`. It previously matched nothing.

**How you'll notice.** A one-time cache miss spike after the upgrade.

If (3) applies to you, you will also see a **warning naming the channel and its
fields**, and that channel will stop caching until the names are corrected. That
is deliberate. A channel whose fields all missed was hashing only method, params
and query, so every request on it collapsed onto one entry and the first
caller's body was served to everyone for the TTL — it was not caching correctly
before, it was mis-serving. Check the field names against your payload shape;
all three spellings above resolve, so in most cases the existing config is
already correct and simply starts working.

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
`trace_queue.buffer_size` (default `1000`) and `trace_queue.max_queue_memory_bytes`
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

### Masking is an allowlist now, and channel auth material is masked too

**What changed.** Connector configs used to be masked by a *denylist* of
secret-looking key names, so a credential under a name the list never
anticipated (`signing_cert_pem`, a custom header value) was served in clear by
`GET /api/v1/admin/connectors`. Masking is inverted: only the structural
vocabulary the connector types define (endpoints, timeouts, operation gates,
identity fields) is readable, and every other value answers `"******"`. All
`headers` *values* are masked — header names stay visible.

Channel configs, which were never masked at all, now mask `auth.keys` and
`auth.secret`. The update path gained the same round-trip handling connectors
have: a masked value sent back on `PUT` is restored from the stored config,
and a sentinel with no stored counterpart is refused with a `400` naming the
field.

**How you'll notice.** Tooling that read non-secret custom keys out of
connector configs via the admin API sees `"******"` where it saw values.
Exports of configs holding *literal* secrets are lossy (they always were for
denylisted names); `env://` references pass through unmasked and remain the
portable way to author credentials.

**What to do.** Nothing for configs authored with `env://` references. If a
custom field must stay readable through the API, it needs to be a real config
field — or fetch it from your own source of truth rather than the masked
admin read.

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
{"error": {"code": "VALIDATION_ERROR",
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

**What to do — delete it.** `ChannelConfig` is `deny_unknown_fields` as of 1.0
(see [Unknown keys in a channel config are now
refused](#unknown-keys-in-a-channel-config-are-now-refused)), so a stored
`config_json` still carrying `queue_depth` **no longer parses**, and the channel
is quarantined at load. This is the same failure as any other unrecognised key.

`orion-server preflight` lists every affected channel. The direct query, if you
would rather look yourself:

```sql
SELECT channel_id, version, name FROM channels
WHERE config_json LIKE '%queue_depth%';
```

While you are in there: the field next to it is named
[`max_concurrent_per_node`](#backpressuremax_concurrent-is-now-max_concurrent_per_node)
as of this release, and the pre-1.0 `max_concurrent` spelling is not accepted
either. A `backpressure` block written for 0.3.0 therefore needs both edits.

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
Ten changes can turn a previously "working" workflow into an explicit error or
a differently-ordered page — in every case the old behaviour was silently
returning wrong or incomplete data. The first fires unconditionally.

- **A task with no `schema` now reaches nothing.** `unmapped` defaulted to
  `identity` — every name passing straight through to the physical one — so a
  dialect task without a `schema` reached every table the connector's database
  user could see, read *and* write. The default is now `reject`. *How you'll
  notice:* **every** `data_query`/`data_write` that declares no `schema` fails
  at its first request. Workflows already stored keep loading and activating;
  nothing fails at startup, so this surfaces on live traffic. The error reads
  `entity '<name>' is not declared in the task's schema: add "schema": … or add
  "unmapped": "identity" …`, and it is the one you get whatever else the query
  mentions, because the entity resolves before the filter, projection and sort.
  *What to do:* one of two things per task —

  ```json
  "schema": { "entities": { "orders": { "columns": { "id": {}, "total": {} } } } }
  ```

  declaring the entities and columns that task uses (the allowlist, and what
  you want long-term), **or** the one-line pass-through that restores 0.x
  behaviour exactly:

  ```json
  "schema": { "unmapped": "identity" }
  ```

  *Find them first:* this is the one change on the page that fails on live
  traffic rather than at startup, so do not discover it from your error rate.
  `orion-server preflight` names every stored task in this shape, workflow and
  task id. The direct query, if you would rather look yourself:

  ```sql
  SELECT name FROM current_workflows
  WHERE tasks_json LIKE '%data_query%' OR tasks_json LIKE '%data_write%';
  ```

  That one over-reports — it cannot see which of those tasks already declares a
  `schema`, which is exactly the part `preflight` does per task.

  Declare every column the task names in `fields`, `sort`, `filter`, `values`,
  `set`, `returning` and `include.<relation>.fields` — the last resolve against
  the *related* entity, so declare its columns too. A bare `{}` is a valid
  declaration when you want no rename or type hint.

  Three things to know once you declare columns. A read that names no `fields`
  now returns exactly the declared **queryable** columns rather than `SELECT *`,
  so `queryable: false` finally hides a column from a field-less read too — an
  entity declaring *no* columns still reads all of them, and one declaring
  columns with every single one non-queryable is refused rather than widened
  back. A relation's `to` target needs no declaration for the relation itself
  to resolve, but does as soon as you name one of its columns — and because an
  `include` must now name a `sort` key, which is a column on that target, **an
  `include` over an undeclared entity cannot plan at all**. And a connector
  owner can refuse the `identity` escape hatch outright with
  `dialect.require_schema`, and bound physical names with
  `dialect.allowed_entities` — see the
  [dialect reference](../reference/data-dialect.md#schema-guards).

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
- **`include` now requires a `sort`, and its page is per parent.** An `include`
  selection without an order key is rejected: the per-parent page is cut inside
  the database (`ROW_NUMBER() OVER (PARTITION BY <fk> ORDER BY <sort>)`), so
  "the first 5 orders" has no defined answer without one — it used to be
  whichever rows the plan emitted, and a different set on the next run.
  **This fails at request time, not at activation:** the dialect envelope is
  not validated when a workflow is activated, so a stored workflow using
  `include` without a `sort` keeps activating and starts failing on live
  traffic. *How you'll notice:* a task fails with `include.<relation> requires
  a 'sort' — the per-parent page needs a deterministic order key`. *What to
  do:* grep your workflows for `"include"` before upgrading and add a `sort` to
  each selection (`"sort": [{"id": "asc"}]` is stable and unsurprising). The
  `sort` may name a column your `fields` does not — it is used for ordering
  only and does not appear in the nested objects. The window function is
  supported by every SQL backend Orion renders for (SQLite ≥ 3.25,
  PostgreSQL ≥ 8.4, MySQL ≥ 8.0), so a MySQL 5.7 server cannot run an
  `include`.

  On MongoDB and Elasticsearch nothing changes: `include` was already rejected
  there with `FeatureUnsupportedByTarget`, and it still is — the sort
  requirement is the SQL planner's, so a doc-store caller still gets the
  capability error that tells them `include` is SQL-only.
- **`include.limit` is bounded by `query.default_limit` / `query.max_limit`,
  per parent.** An `include` with no `limit` used to fetch *every* child of
  every parent on the page and truncate in memory; it now fetches
  `default_limit` (100) children **for each parent row**. A `limit` above
  `query.max_limit` (1000) is rejected with a limit-exceeded error, never
  clamped — the same rule the envelope's own `limit` has always had. *How
  you'll notice:* a task fails with `requested limit N exceeds the configured
  maximum M`, or a nested array that used to be complete now stops at 100
  entries. *What to do:* set an explicit `include.limit`, or raise
  `query.max_limit` (`ORION_QUERY__MAX_LIMIT`). The page is now bounded at
  `parents × include.limit` rows overall, which is the point.
- **Null ordering is inverted on SQL and Elasticsearch: a null sorts as the
  *smallest* value.** Nulls come first on `asc` and last on `desc`. SQL
  emulated "nulls last on `asc`" (with an `IS NULL` prefix sort key on MySQL)
  and Elasticsearch set `"missing": "_last"`, while MongoDB's `find` cannot
  express that rule at all — so the same envelope paged differently on Mongo,
  silently, against a documented promise of deterministic ordering. The shared
  rule is now the one every backend states natively, so the other four move to
  meet Mongo. *How you'll notice:* nothing errors — pages sorted on a nullable
  column come back in a different order, and `skip`-based paging over such a
  column visits rows in a different sequence. *What to do:* if the position of
  nulls matters, filter them out (`{"!=": [{"field": "col"}, null]}`) or sort
  on a non-nullable column first.
- **MongoDB no longer maps `id` to `_id` for you.** Any physical name equal to
  `id` — in filters, projections, sorts, inserted documents, `set` clauses and
  `on_conflict` targets — used to be rewritten to `_id`, so a schema
  deliberately mapping a key onto `id` meant `_id`, and a collection with a
  genuine non-key `id` field was unqueryable. Elasticsearch documented the
  opposite rule two files away; both document stores now pass names through
  exactly as the schema resolved them. *How you'll notice:* a Mongo filter or
  projection on `id` matches nothing where it used to hit the document key —
  documents written before the upgrade carry theirs in `_id`. *What to do:*
  declare the rename, which is what Elasticsearch already required and is also
  what makes inserts carry the id and upsert-on-`id` legal:

  ```jsonc
  "schema": { "entities": { "users": { "columns": { "id": { "name": "_id" } } } } }
  ```

  Without it, `id` is an ordinary field on every backend. If your collection
  genuinely has an ordinary `id` field beside `_id`, do nothing — it is
  queryable now, which it was not before.
- **Every `data_write` result carries a `status`, and a partial bulk is no
  longer an error.** Results gained `"status": "ok"`, so anything asserting on
  the exact result object (`{"rows_affected": 1}`) sees one extra key. A bulk
  `insert` means three different things underneath:

  | Backend | Model | On failure |
  |---|---|---|
  | SQL | **Atomic** | Every row or none — now in an explicit transaction rather than by accident of the renderer's shape |
  | MongoDB | **Prefix-applied** | `insert_many` is ordered: it stops at the first rejected document, commits everything before it, and never attempts the rest |
  | Elasticsearch | **Arbitrary-applied** | `_bulk` attempts every action independently, so any subset can land |

  All three used to return one row count or one opaque error, so documents had
  been written and the caller could not tell which. On MongoDB and
  Elasticsearch a bulk that applied *some* of its rows now returns
  `"status": "partial"` with a per-item array indexed by your `values` array,
  and the task reports audit status **207** instead of failing:

  ```json
  {
    "status": "partial",
    "inserted": 2, "failed": 1, "skipped": 2,
    "ids": ["a", "c"],
    "items": [
      { "index": 0, "status": "ok", "id": "a" },
      { "index": 1, "status": "ok", "id": "c" },
      { "index": 2, "status": "error", "error": { "code": 11000, "message": "duplicate key" } },
      { "index": 3, "status": "skipped" },
      { "index": 4, "status": "skipped" }
    ]
  }
  ```

  `failed`, `skipped` and `items` appear only when there is something to
  report, so a clean bulk keeps the shape it had plus `status`. `skipped` means
  the backend never attempted the item, which only ordered MongoDB produces.
  *What to do:* a workflow that previously relied on the task erroring to halt
  the pipeline now continues — branch on `status` and compensate, using `items`
  to name exactly which indices to retry or roll back. A bulk where *nothing*
  landed is still a hard error, and SQL connectors are unaffected.

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

### Unknown keys in a channel config are now refused

**What changed.** `ChannelConfig` rejects keys it does not recognise. Before
1.0 they were silently ignored. The refusal applies at every nesting level:
a typo *inside* a guard's body — `rate_limit`, `cache`, `deduplication`,
`tracing`, `backpressure`, `auth`, `response` — fails the same way, where it
previously fell back to that field's default (a misspelled
`rate_limit.key_logic` silently meant per-client-IP keying; a misspelled
`deduplication.window_secs` silently took the default window).

**Why.** Every key in a channel config is a *guard*. A key Orion does not
recognise is a guard that never runs — and because nothing re-serialises
`config_json` (the stored document is the one you wrote), the mistake survives
every reload. A stored `"deduplicaton"` meant no idempotency, no error, forever.
The config file, the connector configs and both dialect envelopes already
rejected unknown keys; channel config was the last surface that did not.

**How you'll notice.** A channel whose stored config carries a stray key is
quarantined at load — refused at every ingress, listed on `/health` and the
admin surface with the reason. On create and update it is a `400` naming the
key. This is also the mechanism behind the `cors`, `max_concurrent` and
`queue_depth` entries elsewhere on this page: all four are the same failure.

**What to do.** Run `orion-server preflight` — it names every stored channel
with an unparseable config and, for the two renames, the key to use instead.

### `route_pattern`, `topic` and `consumer_group` are capped at 255 characters

**What changed.** Create, update and import reject values longer than 255
characters in these three fields (field error code `TOO_LONG`). Before 1.0
there was no length check at all.

**Why.** MySQL stores all three columns as `varchar(255)`; SQLite and
Postgres use unbounded `text`. A longer value stored fine on two backends
and failed on the third — a silent divergence the portable schema exists to
prevent. The narrowest backend sets the limit (characters, not bytes).

**How you'll notice.** Only if you write a value that long: a `400` naming
the field. Stored rows are not re-checked at load — a pre-1.0 row over the
limit (only possible on SQLite/Postgres) keeps serving, but its next edit
must shorten the value.

### `backpressure.max_concurrent` is now `max_concurrent_per_node`

**What changed.** The limit was always per node (N replicas admit up to N× the
value); the name now says so, which matters because dedup and rate limiting sit
in the same config block and *are* cluster-shared.

**What to do.** Rename the key on every stored channel that sets it, **and
check the value**. The old spelling is refused — a stored config using it fails
to parse and the channel is quarantined at load.

There is no alias, deliberately. Honouring `max_concurrent` under a field that
means something else would admit N× the intended concurrency on an N-replica
deployment, silently, which is a worse outcome than a channel that refuses to
start. If your 0.3.0 value was sized as a cluster-wide cap, divide it by your
replica count rather than copying it across.

`orion-server preflight` lists every affected channel.

### A channel's `cors` is now `origin_allow_list`

**What changed.** The per-channel key is renamed and flattened:

```json
{ "cors": { "allowed_origins": ["https://app.example.com"] } }
```

becomes

```json
{ "origin_allow_list": ["https://app.example.com"] }
```

**The old spelling is refused.** A stored channel still carrying it fails to
parse and is quarantined at load — refused at every ingress rather than served.
This is deliberate and it is the security-relevant choice: had the old key been
parsed and dropped, the channel would have served with **no origin allow-list at
all**, which is indistinguishable from a channel that deliberately checks
nothing. Every unlisted origin would have been admitted, silently and
permanently. A quarantined channel is the loud version of the same event.

Find them before you upgrade:

```bash
orion-server preflight
```

or directly:

```sql
SELECT name FROM current_channels WHERE config_json LIKE '%"cors"%';
```

**Why it is not cosmetic.** This is a **server-side allow-list**, not CORS: it
sets no `Access-Control-*` header and takes no part in the preflight handshake,
which the platform `[cors]` layer performs for every route *before* a channel
is resolved. The consequence is that a channel's list can only narrow the
platform policy, never widen it — an origin `[cors] allowed_origins` rejects
fails the preflight and never reaches the channel, so listing it on the channel
does nothing. If per-channel origins are not taking effect in a browser, set
`[cors] allowed_origins` to the union of what your channels accept and narrow
from there.

### Audit log: new actor format, new fields, two new settings

**The `principal` column changes format for authenticated callers.** It was the
first eight characters of the presented API key (or of its `sha256:` digest);
it is now a derived `key-<16 hex>` — `SHA-256("orion:audit:key-id:v1" ‖
SHA-256(key))` truncated to 8 bytes. Three things that buys you:

- Two keys sharing a prefix are now two actors. Any generator with a fixed
  leader (`orion_sk_…`) previously collapsed every key into one.
- The audit log no longer contains eight literal characters of a live
  credential.
- The id is the same whether a key is configured in plaintext or `sha256:`
  form, so rotating an operator between the two does not rename them in the
  trail.

Hold the config and you can recompute the id for each key you issued and map a
row back to it; nobody else can go in either direction. **Rows written before
the upgrade keep their old values,** so a saved `?principal=` filter matches
those rows and matches nothing new.

**`details` now carries request context** as a JSON object: `request_id` (the
same value as the `x-request-id` header and the `error.request_id` the client
was handed), `client_ip` (resolved with the `rate_limit.trusted_proxies`
policy, so a forged `X-Forwarded-For` cannot dictate it — and note that policy
now applies even with `rate_limit.enabled = false`, so a proxied deployment
records the caller rather than the load balancer) and `user_agent` (truncated
to 256 bytes). Unavailable fields are omitted rather than recorded empty. It
previously held `{"request_id": …}` at most.

**Mutations immediately before a restart are now recorded.** The write was a
detached task nothing awaited, so a mutation accepted moments before `SIGTERM`
was answered `200` and then lost — the row an investigation of a bad deploy
most wants. It now goes onto a bounded queue drained at shutdown. Two new
settings, both with working defaults:

| Setting | Default | Raise it when |
|---|---|---|
| `audit.max_pending` | `1000` | A bursty admin plane (large `/import` batches) overruns the writer |
| `audit.drain_timeout_secs` | `5` | Shutdown reports abandoned rows on a slow database |

Both are refused at `0`. Anything that still does not make it is counted in
`orion_audit_events_dropped_total{reason}` (`queue_full`, `write_failed`,
`drain_timeout`, `writer_stopped`) and logged at `error`. **Alert on that
counter existing at all, not on a threshold** — any non-zero value is a hole in
the audit trail.

**`POST /admin/workflows/{id}/test` now writes an `action: "test"` row.** It
reads as a dry run and is not one: it executes the workflow's tasks against
live connectors. If you have an audit-volume alert, expect it to see traffic
from this endpoint for the first time.

### Startup retries an unreachable database instead of exiting

**What changed.** A database that was down or mid-failover at boot used to be a
hard exit: `.connect()` is eager and `min_connections = 5` requires five live
connections before boot succeeds, so every replica crash-looped for the whole
failover and the container restart backoff outlived it. Startup now retries the
initial connection with a 250 ms → 5 s exponential backoff, bounded by the new
`storage.connect_retry_secs` (default `60`).

**How you'll notice.** A genuinely wrong `storage.url` or an unreachable host
now takes up to ~60 s to fail instead of ~3 s, with one `WARN` line per attempt
naming the error and the next backoff.

**What to do.** Usually nothing — the readiness probe already keeps traffic off
a pod that has not finished booting, and the default window is sized to ride
out a typical PostgreSQL failover. Set `storage.connect_retry_secs = 0` to
restore fail-fast where a fast exit is the point: pre-flight smoke tests, CI
health gates, init containers that only check connectivity. Two things are
unaffected: SQLite is never retried (a bad path, bad permissions or a corrupt
file does not heal on its own), and the pending-migration refusal under
`auto_migrate = false` is still immediate — it is about schema state, not
reachability.

### Two reload warnings no longer repeat while the condition persists

The channel registry now carries unchanged channels and an unchanged route
table across a reload instead of rebuilding them. Two warnings were emitted as
a side effect of that rebuild and therefore repeated on every reload:

- `Two active channels claim the same route …` from the route-table build
  (fields `route`, `shadowed_channel`, `serving_channel`), now skipped when the
  serviceable channel set is unchanged.
- `<purpose> connector unavailable, falling back to in-memory` for a channel
  whose dedup or response-cache connector could not be resolved (single-node
  mode only — cluster mode quarantines instead), now not re-logged for a
  channel that was carried over.

Both conditions are still logged on the reload that introduces or changes them,
and both remain visible in the state they describe (`/health` for quarantined
channels, the admin API's validation for route conflicts). If you alert on the
*recurrence* of either line rather than on its first appearance, switch to a
first-occurrence or state-based alert.

### Connector operation gates now cover every connector type

Additive and fully backward compatible — existing connectors behave exactly as
before, since every gate defaults to allowed. If you want the new locks:

```json
{ "type": "cache", "backend": "redis", "url": "redis://…", "operations": { "write": false } }
{ "type": "kafka", "brokers": ["…"], "topic": "t", "operations": { "publish": false } }
{ "type": "http",  "url": "https://partner.example.com/v1", "operations": { "methods": ["GET"] } }
```

The HTTP allow-list is exhaustive once non-empty and matches
case-insensitively; a method outside `GET`, `POST`, `PUT`, `PATCH`, `DELETE` is
rejected with a `400` when the connector is created or updated, as is a gate
key the type does not have. A gated call fails with the same validation error
the `db`/`es` gates produce.

One interaction worth knowing: a `cache` connector's `write` gate covers every
write through it, **including a channel dedup store or response cache backed by
it** — so gating a shared Redis read-only makes any channel pointing its dedup
store at that connector fail to load, rather than silently downgrading. There
is no `delete` gate on `cache`: the backend trait has no delete.

### OpenAPI schema components renamed

Five response schemas in `docs/openapi.json` changed name. For the first four
**no response body changed** — the JSON field sets are identical — so only
clients generated from the spec, which take their type names from component
names, are affected:

| Before | After |
| --- | --- |
| `Connector` | `ConnectorResponse` |
| `AuditLogEntry` | `AuditLogEntryResponse` |
| `TraceDlqEntry` | `TraceDlqEntryResponse` |
| `PaginatedEnvelope_TraceDlqEntry` | `PaginatedEnvelope_TraceDlqSummaryResponse` |
| `PaginatedEnvelope_TraceListItem` | `TracePageEnvelope` |

The fifth is different: the trace-list envelope was renamed because its
*shape* changed, not its row type. `total` is now conditional and
`next_cursor` is new — see
[the trace-list section](#the-trace-list-no-longer-returns-total-by-default).

The generic envelope names follow (`DataEnvelope_Connector` →
`DataEnvelope_ConnectorResponse`, and so on). Regenerate your client and rename
the referenced types; no field access changes.

The last row is a correction rather than a rename. `GET /api/v1/admin/trace-dlq`
has never returned `payload_json` or `metadata_json` — it selects a
payload-free projection so one request cannot dump every failed request's body
— but the published schema claimed both fields, because the row struct that
*did* have them was also the wire type. If you generated a client that modelled
DLQ list rows as carrying payloads, those fields were always absent at runtime.
Fetch a single entry with `GET /api/v1/admin/trace-dlq/{id}` for the payload.

**One error code changed with it:** a database failure while listing audit logs
now returns `{"error": {"code": "STORAGE_ERROR"}}` instead of
`INTERNAL_ERROR`. The status is still 500. That is what every other list
endpoint already returned — and what the *count* half of this same query
already returned.

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

- **Hashing does not change the audit `principal`,** and that is the point: the
  actor is a `key-<16 hex>` derived from the key, identical whether the entry
  is configured in plaintext or `sha256:` form, so rotating an operator between
  the two does not rename them in the trail. See
  [Audit log](#audit-log-new-actor-format-new-fields-two-new-settings) for the
  format change itself, which does break saved `?principal=` filters.
- **A plaintext key whose literal text starts with `sha256:`** is now
  interpreted as the hash-at-rest form and will fail config validation. Rotate
  it first.

**Enable admin auth** if you have not. It is what guards `/metrics`, the trace
endpoints, and therefore the full-detail error payloads.

**Enable the Kafka DLQ** — see [section 4](#4-kafka-delivery-is-now-at-least-once).

**Set `storage.auto_migrate = false`** in any multi-replica deployment and run
`orion-server migrate` as a deploy step. In a *production* cluster this is no
longer advice — see
[A production cluster may not migrate at boot](#a-production-cluster-may-not-migrate-at-boot).

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
