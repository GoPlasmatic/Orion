<!-- description: Behaviour Orion 1.0 changed without renaming anything: sticky rollouts, cache keys and namespaces, trace batching, startup retries and readiness. -->
<!-- type: migration -->
<!-- last_verified: 2026-09-14 -->

# Runtime behaviour

Break 11 of eleven in the 0.3.0 → 1.0.0 upgrade.

## Before you start

Read [Upgrade to 1.0.0](./index.md) first: it carries the checklist, the backup step and the `preflight` scan.

Same contract, different behaviour underneath. Nothing in this group changes a request or response shape.

### Sticky canary rollouts are now caller-stable

**What changed.** The rollout bucket was `rand::random` per request. A caller in a 10% canary flip-flopped between versions call to call, and replica to replica. The bucket is now a stable hash of a caller identity. That is `engine.rollout_sticky_header` when configured, for example `x-user-id`, and otherwise the forwarded client IP (`X-Forwarded-For` first element, else `X-Real-IP`). Requests with no identity keep the random fallback, so percentages still hold in aggregate.

**How you'll notice.** A given caller now consistently gets the same version.
The *population* split still matches the configured percentage, but it is no longer re-drawn per request. A 10% canary that previously exposed nearly every caller occasionally now exposes a stable 10% of callers.

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

### Response cache keys changed format

**What changed.** Three things, all of which change the hash:

1. The key hashed only the request body. It now also folds in the HTTP method,
   route parameters, and query string (both sorted, so ordering does not affect
   the key). The old key could serve one caller's cached response to a different
   request that happened to share a body.
2. The digest is **SHA-256 truncated to 128 bits**, not FNV-1a. FNV-1a is a
   multiply/xor over 64 bits with no collision resistance — a colliding payload
   is constructed rather than searched for, and the data plane is unauthenticated
   by design, so on most deployments the body is attacker-shaped input. Two
   requests that hash alike are served each other's response bodies.
3. `cache_key_fields` entries now resolve as **paths**, not only literal
   top-level keys. `user.id` walks into a nested object, and `data.user_id` —
   the spelling this guide and the feature docs have always shown — resolves to
   the payload's `user_id`. It previously matched nothing.

**How you'll notice.** A one-time cache miss spike after the upgrade.

If (3) applies to you, you will also see a **warning naming the channel and its fields**. That channel stops caching until the names are corrected. That is deliberate. A channel whose fields all missed was hashing only method, params and query. Every request on it collapsed onto one entry and the first caller's body was served to everyone for the TTL. It was not caching correctly before, it was mis-serving. Check the field names against your payload shape. All three spellings above resolve, so an existing config that names real fields starts working unchanged.

**What to do.** Nothing is required. The key prefix (`cache:{channel}:{hash}`) is unchanged and carries no version segment. Old entries are therefore **orphaned, not mis-served**: a new request cannot reproduce an old hash. They expire on their own through `cache.ttl_secs` (default `300` seconds when unset). There is no cache-flush endpoint; for a guaranteed-clean cutover on Redis:

```bash
redis-cli --scan --pattern 'cache:*' | xargs -r redis-cli DEL
```

With the in-memory backend, entries are process-local and a restart clears them.

### Workflow caches, dedup stores and response caches no longer share one keyspace

**What changed.** Every `backend: "memory"` cache connector, plus the built-in
dedup store and response cache, shared a single in-process instance and one LRU budget. In-memory backends are now separate instances per purpose (workflow cache / dedup / response cache) and per connector name, each with its own `engine.max_memory_cache_entries` budget.

**How you'll notice.** Only if something depended on the aliasing. A workflow `cache_read` can no longer observe dedup or response-cache entries, or another memory connector's keys. A workflow `cache_write` can no longer influence dedup or response-cache decisions. Memory contents never survived a restart, so there is no data migration.

**What to do.** Re-check your sizing if the host is memory-constrained. The setting `engine.max_memory_cache_entries` is now a **per-namespace** bound. The worst case is that value × (2 built-in stores + up to 3 namespaces per memory connector). Divide the setting by your namespace count to keep the old ceiling.

Redis cache connectors are deliberately *not* partitioned: pointing a workflow connector and a channel's dedup store at the same Redis database still shares a keyspace. Use separate databases (`redis://host/0`, `/1`, …) where you need isolation.

### `trace_storage.batch_size` now defaults to `1000`

**What changed.** Only the default. `trace_storage.mode` still defaults to `sync`, so a deployment that has not opted into `batch` or `async` is unaffected.

For deployments that *have*, a flush costs a fixed per-transaction price plus a per-row one. The old default of `100` rows per flush spent most of each transaction on overhead. Measured on SQLite with 4 workers, the same load drained at 26k rows/s at `100` and 45k rows/s at `1000`. That is a tenth as many transactions for the same rows.

**How you'll notice.** `batch` and `async` modes keep up with a higher request
rate before `max_pending` overruns, and `orion_trace_persistence_batch_size` reports larger flushes. Trace visibility is unchanged — a partial batch still flushes on `batch_flush_interval_ms`.

**What to do.** Nothing. Set `batch_size` explicitly to pin the old value:

```toml
[trace_storage]
batch_size = 100
```

### Trace loss under `batch` / `async` now warns in the log

**What changed.** When the persistence queue overruns `max_pending`, the dropped
traces were reported only to `orion_trace_dropped_total{reason="overflow"}` — and `metrics.enabled` defaults to `false`. The out-of-the-box signal for "your traces are being discarded" was a counter nobody was collecting. The drop now also logs a `WARN`, immediately when the loss starts and then at most once every 5 seconds. Each line carries how many traces were dropped since the previous one.

**How you'll notice.** A log line naming the overrun, if you run `batch` or `async` at a request rate the database cannot absorb. The `sync` mode cannot produce it.

**What to do.** Treat the line as real data loss, not noise. Raise `trace_storage.max_pending` or `batch_size`. Set `async_on_overflow = "block"` to slow producers instead of shedding, or sample deliberately with `sample_rate` and `errors_only`. The last option is `mode = "sync"`, which lets the trace table throttle the request path rather than be outrun by it.

### Async submissions are exempt from trace sampling

**What changed.** Channels with `trace_storage.sample_rate < 1.0` serving
`/async` traffic used to write the trace's status rows but drop its *result*. The trace came back `completed` with nothing in it, and the storage was spent anyway. The result is now always persisted for an async submission. The 202's `trace_id` is a receipt for a fetchable result, exactly as `mode = "off"` is already upgraded to `sync` on that path.

**What to do.** If you used `sample_rate` to bound async trace storage, switch
to `errors_only = true` or tighten `trace_queue.retention_hours`. The sync path samples exactly as configured, and a sampled-out sync trace now leaves no row at all.

### Storage pool defaults: a docs correction, not a behaviour change

**No runtime default changed between 0.3.0 and 1.0.0.** The 0.3.0 *documentation* disagreed with the code. The config reference said `max_connections = 25` and `config.toml.example` said `10`, while the code has always defaulted to `50`. The docs are now generated from the code.

**What to do.** If you sized your database against the documented number, check
it against the real one. The actual defaults are:

| Key | Default |
|-----|---------|
| `storage.max_connections` | `50` |
| `storage.min_connections` | `5` |
| `storage.acquire_timeout_secs` | `3` |
| `storage.idle_timeout_secs` | `300` |
| `storage.busy_timeout_ms` | `5000` (SQLite only) |

In cluster mode this multiplies: *replicas × `max_connections`* must fit inside your PostgreSQL `max_connections`, minus headroom for the migration job and your own tooling.

### Startup retries an unreachable database instead of exiting

**What changed.** A database that was down or mid-failover at boot used to be a hard exit. The `.connect()` call is eager, and `min_connections = 5` requires five live connections before boot succeeds. Every replica crash-looped for the whole failover and the container restart backoff outlived it. Startup now retries the initial connection with a 250 ms → 5 s exponential backoff, bounded by the new `storage.connect_retry_secs` (default `60`).

**How you'll notice.** A genuinely wrong `storage.url` or an unreachable host now takes up to ~60 s to fail instead of ~3 s. One `WARN` line per attempt names the error and the next backoff.

**What to do.** Usually nothing. The readiness probe already keeps traffic off a pod that has not finished booting. The default window is sized to ride out a typical PostgreSQL failover. Set `storage.connect_retry_secs = 0` to restore fail-fast where a fast exit is the point: pre-flight smoke tests, CI health gates, init containers that only check connectivity. Two things are unaffected: SQLite is never retried (a bad path, bad permissions or a corrupt file does not heal on its own). The pending-migration refusal under `auto_migrate = false` is still immediate. It is about schema state, not reachability.

### Two reload warnings no longer repeat while the condition persists

The channel registry now carries unchanged channels and an unchanged route table across a reload instead of rebuilding them. Two warnings were emitted as a side effect of that rebuild and therefore repeated on every reload:

- `Two active channels claim the same route …` from the route-table build
  (fields `route`, `shadowed_channel`, `serving_channel`), now skipped when the
  serviceable channel set is unchanged.
- `<purpose> connector unavailable, falling back to in-memory` for a channel
  whose dedup or response-cache connector could not be resolved (single-node
  mode only — cluster mode quarantines instead), now not re-logged for a
  channel that was carried over.

Both conditions are still logged on the reload that introduces or changes them. Both remain visible in the state they describe: `/health` for quarantined channels, and the admin API's validation for route conflicts. If you alert on the
*recurrence* of either line rather than on its first appearance, switch to a
first-occurrence or state-based alert.

### Workflow export reads in bounded pages

`GET /api/v1/admin/workflows/export` still returns every matching workflow in one response; it now reads the database in bounded 500-row pages instead of one unbounded query.

**One caveat if you use export as a backup:** it is no longer a point-in-time
snapshot. The pages are independent queries, so a workflow created, deleted or renamed during an export can be missed or appear twice in a single response. Quiesce workflow mutations during export, or re-export until two consecutive responses match, when you need a consistent copy.

If you embed Orion as a library, `WorkflowRepository::list` now honours its filter's `limit`/`offset` (default 50, max 1000 per call) instead of returning the whole table. Page through it if you need everything.

### `validate-config` prints the full effective config

**What changed.** `orion-server validate-config` no longer prints the old
hand-maintained summary of a dozen settings. By default it prints the *full effective config* as TOML on stdout: every section, merged from defaults, the config file and `ORION_*` overrides. Secrets are masked, with `******` for key-named secrets and passwords struck out of URL-shaped values such as `storage.url`. Under `--format toml` and `--format json` the `Configuration is valid.` note goes to stderr so stdout stays machine-parseable; `--format summary` keeps it on stdout.

**How you'll notice.** Deploy scripts that grep the old summary (`:8080`-style
host:port lines, `storage: sqlite:orion.db`) stop matching. Anything that read a database password out of the old output stops working. That was a credential leak, and it is masked in every format now.

**What to do.** Parse stdout as TOML, or run `--format json` and parse JSON;
`--format summary` restores a short human-readable summary (also masked). Exit codes are unchanged, so plain pre-flight checks (`validate-config || exit 1`) need no change.

### `/readyz` and `/health` observe Kafka ingestion

**What changed.** With `kafka.enabled = true`, both probes gain a
`components.kafka` field, and **`/readyz` returns 503 while ingestion is degraded**. That is, a consumer (re)start failed and the built-in restart supervisor has not yet brought one back. The supervisor is new in 1.0, with backoff capped at 1 s → 60 s. Previously a node in that state reported ready while consuming nothing. `/health` reports `status: "degraded"` while HTTP itself keeps serving.

**What to do.** If your readiness alerting assumed only the database, engine or startup could unready a node, account for the new component. A degraded node now leaves the load-balancer rotation. The `orion_kafka_ingest_degraded` gauge (0/1) carries the same signal for Prometheus. Deployments with Kafka disabled see byte-identical probe bodies.

### Optional: fail closed when a guard's backend is down

Two new per-channel settings arrive: `rate_limit.on_backend_error` and `deduplication.on_backend_error`. Each accepts `"allow"`, the default and today's fail-open behaviour, or `"deny"`, which refuses requests with `503` while the guard's backend cannot answer. Never a `409` or `429`, because the key or limit is unverifiable rather than violated. Nothing changes unless you set it. Consider `"deny"` on payment or idempotency-critical channels, where a Redis blip silently removing all idempotency is worse than refusing the request.

### New operational settings, no action required

- **`storage.backup_retention_count`**: unset by default, which keeps every
  backup (the pre-1.0 behaviour). Set it to bound SQLite backups: after each
  successful `POST /api/v1/admin/backups` the oldest `orion_backup_*.db` files
  are pruned so at most N remain. `0` is refused at startup. Env override
  `ORION_STORAGE__BACKUP_RETENTION_COUNT`; set it to an empty string to clear.
- **`orion_job_last_success_timestamp_seconds{job}`**: a gauge for the
  background jobs (`trace_cleanup`, `audit_cleanup`, `dlq_retry`,
  `epoch_watcher`, `kafka_lag`). Alert on
  `time() - orion_job_last_success_timestamp_seconds{job="…"}` exceeding a few
  tick intervals: the jobs swallow per-tick errors by design, so this gauge
  going stale is the only signal that cleanup or DLQ retry has silently
  stalled.

---

## Related

- [Upgrade to 1.0.0](./index.md): the checklist, and every other break.
- [Upgrades](../../operate/maintain/upgrades.md): the version-independent procedure.
- [`orion-server preflight`](../../reference/cli/orion-server/preflight.md): the scan that finds the stored ones.
- [Releases](./index.md): what changed in each version.
