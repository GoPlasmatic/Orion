<!-- description: Every field, key and endpoint Orion 1.0 renamed or removed: the data_write envelope, response_path, cors, backpressure and the trace read routes. -->
<!-- type: migration -->
<!-- last_verified: 2026-09-14 -->

# Renames and removals

Break 8 of eleven in the 0.3.0 → 1.0.0 upgrade.

## Before you start

Read [Upgrade to 1.0.0](./index.md) first: it carries the checklist, the backup step and the `preflight` scan.

Old spellings are refused rather than silently accepted, so each of these fails loudly at the surface that owns it. See [How a rename fails, by surface](../../operate/maintain/upgrades.md#how-a-rename-fails-by-surface).

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

**How you'll notice.** The flat form is **not** accepted. The `write` key is a required input, so a task still in the old shape is refused with an error naming it. That happens at create, update, bulk import, `POST /admin/workflows/validate` and `orion-server lint`. A workflow already stored in the flat shape fails at its first request.

Find them before you upgrade:

```bash
orion-server preflight
```

**What to do.** Move the eight envelope keys into a `write` object: `op`, `target`, `values`, `set`, `filter`, `on_conflict`, `returning` and `all`. Leave `connector`, `schema`, `params`, `database` and `output` where they are. Stale flat keys left behind by a half-finished migration are inert — `write` is the only envelope. You can move them one workflow at a time.

**Why.** The two halves of one dialect read differently. The envelope shared a namespace with the handler, so it could never grow a field named `connector`, `schema`, `params`, `database` or `output`. Nesting also means there is one JSON value that *is* the envelope. Validation errors now point at `…function.input.write.target` instead of a path that could mean either half.

### `response_path` is now called `output`

**What changed.** Eight of the ten connector functions named their destination
path `output`; `http_call` and `channel_call` named it `response_path`. All ten now take `output`.

**How you'll notice.** You won't — `response_path` is still accepted, so
existing workflows keep running. Unlike the other 1.0 renames it carries no removal date. On `http_call` the alias belongs to the `HttpCallConfig` struct in `dataflow-rs`, which Orion does not own and cannot remove on its own. It is listed under [accepted alternate spellings](../versioning-policy.md#accepted-alternate-spellings) rather than as a deprecation. Supplying both keys is a duplicate-field error, not a precedence rule.

**What to do.** Rename the key at your leisure:

```json
// before
{ "name": "http_call", "input": { "connector": "crm", "response_path": "data.customer" } }
// after
{ "name": "http_call", "input": { "connector": "crm", "output": "data.customer" } }
```

The *defaults* are unchanged and still differ by function: omitting `output` on `http_call` discards the response, while every other handler writes to `"data"`.

### A channel's `cors` is now `origin_allow_list`

**What changed.** The per-channel key is renamed and flattened:

```json
{ "cors": { "allowed_origins": ["https://app.example.com"] } }
```

becomes

```json
{ "origin_allow_list": ["https://app.example.com"] }
```

**The old spelling is refused.** A stored channel still carrying it fails to parse and is quarantined at load. It is refused at every ingress rather than served. This is deliberate, and it is the security-relevant choice. Had the old key been parsed and dropped, the channel would have served with **no origin allow-list at all**. That is indistinguishable from a channel that deliberately checks nothing. Every unlisted origin would have been admitted, silently and permanently. A quarantined channel is the loud version of the same event.

Find them before you upgrade:

```bash
orion-server preflight
```

or directly:

```sql
SELECT name FROM current_channels WHERE config_json LIKE '%"cors"%';
```

**Why it is not cosmetic.** This is a **server-side allow-list**, not CORS. It sets no `Access-Control-*` header and takes no part in the preflight handshake. The platform `[cors]` layer performs that handshake for every route *before* a channel is resolved. The consequence is that a channel's list can only narrow the platform policy, never widen it. An origin `[cors] allowed_origins` rejects fails the preflight and never reaches the channel, so listing it on the channel does nothing. If per-channel origins are not taking effect in a browser, set `[cors] allowed_origins` to the union of what your channels accept and narrow from there.

### `backpressure.max_concurrent` is now `max_concurrent_per_node`

**What changed.** The limit was always per node: N replicas admit up to N× the value. The name now says so, which matters because dedup and rate limiting sit in the same config block and *are* cluster-shared.

**What to do.** Rename the key on every stored channel that sets it, **and
check the value**. The old spelling is refused — a stored config using it fails to parse and the channel is quarantined at load.

There is no alias, deliberately. Honouring `max_concurrent` under a field that means something else would silently admit N× the intended concurrency on an N-replica deployment. That is a worse outcome than a channel refusing to start. If your 0.3.0 value was sized as a cluster-wide cap, divide it by your replica count rather than copying it across.

`orion-server preflight` lists every affected channel.

### `backpressure.queue_depth` was removed

**What changed.** The field was parsed but never read. Backpressure rejects
immediately at `max_concurrent` through `try_acquire`; there is no wait queue, so the field promised behaviour that never existed.

**What to do — delete it.** `ChannelConfig` is `deny_unknown_fields` as of 1.0
(see [Unknown keys in a channel config are now refused](./api-and-response-shape.md#unknown-keys-in-a-channel-config-are-now-refused)). A stored `config_json` still carrying `queue_depth` **no longer parses**, and the channel is quarantined at load. This is the same failure as any other unrecognized key.

`orion-server preflight` lists every affected channel. The direct query, if you would rather look yourself:

```sql
SELECT channel_id, version, name FROM channels
WHERE config_json LIKE '%queue_depth%';
```

While you are in there: the field next to it is named [`max_concurrent_per_node`](#backpressuremax_concurrent-is-now-max_concurrent_per_node) as of this release. The pre-1.0 `max_concurrent` spelling is not accepted either. A `backpressure` block written for 0.3.0 therefore needs both edits.

### `engine.reload_timeout_secs` and `orion_engine_lock_wait_seconds` are gone

**What changed.** The live engine was held behind a read-write lock. Every request acquired a read guard, and a reload waited for a write guard. It is published with an atomic store now, so readers never block and a reload never waits.

Two things existed only to describe that wait and have been removed:

- **`engine.reload_timeout_secs`** (`ORION_ENGINE__RELOAD_TIMEOUT_SECS`) — how
  long a reload would wait for the write lock. There is no wait to bound.
- **`orion_engine_lock_wait_seconds`**: the histogram of that wait. It could
  now only ever report zero.

The `_orion.profile` debug output loses its `engine_lock_wait` phase and `engine_lock_wait_ms` field for the same reason. `engine.health_check_timeout_secs` stays — it still bounds the `/readyz` cluster-Redis ping.

**How you'll notice.** Setting `ORION_ENGINE__RELOAD_TIMEOUT_SECS` now **stops
the boot** with a message naming it as removed, rather than being silently ignored. A `reload_timeout_secs` line in a config file is rejected by `deny_unknown_fields` the same way.

**What to do.** Delete the setting from any config file, Helm values or
environment. Drop `orion_engine_lock_wait_seconds` from dashboards and alerts — a panel on it reads empty rather than breaking.

### The trace read endpoints moved to the admin plane

**What changed.** Both trace endpoints moved:

| Before | After |
|---|---|
| `GET /api/v1/data/traces` | `GET /api/v1/admin/traces` |
| `GET /api/v1/data/traces/{id}` | `GET /api/v1/admin/traces/{id}` |

**There is no redirect.** The old paths now resolve as *channel* names on the data-plane catch-all. A request to one returns 404 rather than a 308, or runs a channel you happen to have named `traces`.

**Why.** The list endpoint was already admin-guarded, so its placement on the
data plane was a naming lie. It was also a functional one. `/traces` and `/traces/{id}` were static routes, and axum resolves static segments before the `/{*path}` catch-all. So **a channel named `traces` was permanently unreachable**, `POST /api/v1/data/traces` returned 405, and the rate limiter carried a special case to skip the name. None of that was documented or checked, and it silently did not work.

**How you'll notice.** Any async client that polls `GET /api/v1/data/traces/{id}`
starts getting 404. Operator tooling hitting the list gets the same.

**What to do.** Update the paths. The access rules are unchanged. The list needs an admin credential, and the single-trace GET takes *either* an admin credential or the submission's `trace_token` (see the next section). It lives under `/api/v1/admin` and is the one path in that namespace not covered by the blanket admin guard.

```bash
# before
curl "http://orion:8080/api/v1/data/traces/$id"  -H "x-trace-token: $tok"
# after
curl "http://orion:8080/api/v1/admin/traces/$id" -H "x-trace-token: $tok"
```

### OpenAPI schema components renamed

Five response schemas in `docs/openapi.json` changed name. For the first four, **no response body changed**. The JSON field sets are identical, so only clients generated from the spec are affected, because those take their type names from component names:

| Before | After |
| --- | --- |
| `Connector` | `ConnectorResponse` |
| `AuditLogEntry` | `AuditLogEntryResponse` |
| `TraceDlqEntry` | `TraceDlqEntryResponse` |
| `PaginatedEnvelope_TraceDlqEntry` | `PaginatedEnvelope_TraceDlqSummaryResponse` |
| `PaginatedEnvelope_TraceListItem` | `TracePageEnvelope` |

The fifth is different: the trace-list envelope was renamed because its
*shape* changed, not its row type. `total` is now conditional and
`next_cursor` is new. See [the trace-list section](./api-and-response-shape.md#the-trace-list-no-longer-returns-total-by-default).

The generic envelope names follow (`DataEnvelope_Connector` → `DataEnvelope_ConnectorResponse`, and so on). Regenerate your client and rename the referenced types; no field access changes.

The last row is a correction rather than a rename. The endpoint `GET /api/v1/admin/trace-dlq` has never returned `payload_json` or `metadata_json`: it selects a payload-free projection, so one request cannot dump every failed request's body. The published schema claimed both fields anyway, because the row struct that *did* have them was also the wire type. If you generated a client that modelled
DLQ list rows as carrying payloads, those fields were always absent at runtime. Fetch a single entry with `GET /api/v1/admin/trace-dlq/{id}` for the payload.

**One error code changed with it:** a database failure while listing audit logs
now returns `{"error": {"code": "STORAGE_ERROR"}}` instead of `INTERNAL_ERROR`. The status is still 500. That is what every other list endpoint already returned, and what the *count* half of this same query already returned.

## Related

- [Upgrade to 1.0.0](./index.md): the checklist, and every other break.
- [Upgrades](../../operate/maintain/upgrades.md): the version-independent procedure.
- [`orion-server preflight`](../../reference/cli/orion-server/preflight.md): the scan that finds the stored ones.
- [Releases](./index.md): what changed in each version.
