<!-- description: Orion 1.0's security changes: the async trace token, admin-only trace reads, sanitised error bodies, allowlist masking and stricter SSRF rules. -->
<!-- type: migration -->
<!-- last_verified: 2026-09-14 -->

# Security and access

Break 9 of eleven in the 0.3.0 → 1.0.0 upgrade.

## Before you start

Read [Upgrade to 1.0.0](./index.md) first: it carries the checklist, the backup step and the `preflight` scan.

Changes to what is exposed, what is masked, and what now needs a credential.

### Polling an async trace now requires the token returned with the 202

**What changed.** `POST /api/v1/data/{channel}/async` returns a `trace_token` alongside `trace_id`. The read endpoint `GET /api/v1/admin/traces/{id}` requires that token, in the `x-trace-token` header or a `?token=` query parameter, unless the caller presents an admin credential. Previously the endpoint was all-or-nothing admin auth. On a default config it was open to everyone, so any caller could read another caller's payloads by walking trace ids. With admin auth on it was closed to the submitter.

The trace *list* (`GET /api/v1/admin/traces`) is unchanged in its auth, but now returns payload-free rows. `input_json`, `result_json` and `task_trace_json` are served only by the single-trace GET. That endpoint's `message` also no longer includes the submitter's request context (`context.metadata`).

**How you'll notice.** A polling client that ignores `trace_token` starts
getting `401` on its next poll.

**What to do.** Capture `trace_token` from the 202 and send it on each poll:

```bash
resp=$(curl -s -X POST http://orion:8080/api/v1/data/orders/async \
  -H 'Content-Type: application/json' -d '{"data":{"order_id":1}}')
id=$(jq -r .trace_id <<<"$resp"); tok=$(jq -r .trace_token <<<"$resp")
curl -s "http://orion:8080/api/v1/admin/traces/$id" -H "x-trace-token: $tok"
```

Operator tooling that already sends an admin key needs no change. Traces created before the upgrade have no token and stay on the admin trust model. The migration adding `traces.access_token_hash` runs automatically on all three backends.

### Trace read endpoints require admin auth

**What changed.** `GET /api/v1/admin/traces` and `GET /api/v1/admin/traces/{id}`
return full input and result payloads but were reachable without a key even with `admin_auth.enabled = true`. They are now guarded alongside `/api/v1/admin/*` and `/metrics`.

**How you'll notice.** Previously open callers polling for async results get
`401`. URLs are unchanged.

**What to do.** Send the admin key on those requests. There is **no effect if `admin_auth.enabled = false`**, still the default. That is also why enabling admin auth is recommended: see the sanitisation gap above.

### `/docs` and the OpenAPI spec are off in production

**What changed.** Swagger UI (`/docs`) and `/api/v1/openapi.json` used to be registered unconditionally and unauthenticated. Every deployment therefore published the complete admin API surface to anonymous callers. They are now gated by `server.docs.enabled`: unset (the default) serves them only when `environment` does not start with `prod`; an explicit `true`/`false` always wins.

**How you'll notice.** With `environment = "production"`, `GET /docs` and
`GET /api/v1/openapi.json` return **404**: not 401. The routes are not registered at all, so their existence is not advertised either.

**What to do.** Usually nothing; this is the intended hardening. If production tooling reads the served spec, set `server.docs.enabled = true` (or `ORION_SERVER__DOCS__ENABLED=true`) to opt back in. The alternative is `orion-server dump-openapi > spec.json`, which works offline whatever this setting says.

### Data-plane error bodies are sanitised

**What changed.** Workflow task errors returned on the data plane used to carry full internal detail. That meant the raw error message, workflow ID, task path and retry state. Each entry in `errors[]` is now reduced to a code, a fixed generic message, and (when present) the task ID:

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

**What to do.** Correlate on `request_id`, which is a **top-level sibling of `errors[]`** rather than a field inside each entry. Fetch the full detail from the persisted trace at `GET /api/v1/admin/traces/{id}`. It is also returned as the `x-request-id` response header. Cached responses store the sanitised body, so a cache hit is consistent with a miss.

> **This is data-plane only, and it has a gap by default.**
> `GET /api/v1/admin/traces/{id}` returns the **unsanitised** result. That
> endpoint is guarded only when `admin_auth.enabled = true`, and the default is
> `false`. Enable admin auth for the sanitisation to hold end to end.

### Credential headers are masked in workflow metadata

**What changed.** `metadata.headers` now carries `"******"` for
`authorization`, `cookie`, `proxy-authorization` and `x-api-key`. Their plaintext values previously reached `traces.result_json` and `trace_dlq.metadata_json`.

**How you'll notice.** `validation_logic` that compares a credential header's
*value* stops matching. Testing header *presence* still works.

**What to do.** If a channel used `rollout.sticky_header` pointing at a credential header, switch it to a non-credential one. Otherwise every caller now hashes into the same rollout bucket. Rows written before the upgrade still contain plaintext headers at rest; the trace-read projection hides them from HTTP responses, and `trace_queue.retention_hours` ages them out.

### Masking is an allowlist now, and channel auth material is masked too

**What changed.** Connector configs used to be masked by a *denylist* of secret-looking key names. A credential under a name the list never anticipated, such as `signing_cert_pem` or a custom header value, was served in clear by `GET /api/v1/admin/connectors`. Masking is inverted: only the structural vocabulary the connector types define (endpoints, timeouts, operation gates, identity fields) is readable, and every other value answers `"******"`. All `headers` *values* are masked — header names stay visible.

Channel configs, which were never masked at all, now mask `auth.keys` and `auth.secret`. The update path gained the same round-trip handling connectors have: a masked value sent back on `PUT` is restored from the stored config. A sentinel with no stored counterpart is refused with a `400` naming the field.

**How you'll notice.** Tooling that read non-secret custom keys out of
connector configs through the admin API sees `"******"` where it saw values. Exports of configs holding *literal* secrets are lossy, as they always were for denylisted names. References written as `env://` pass through unmasked, and remain the portable way to author credentials.

**What to do.** Nothing for configs authored with `env://` references. If a custom field must stay readable through the API, it needs to be a real config field. Otherwise fetch it from your own source of truth rather than the masked admin read.

### Connector reads redact credentials inside URLs

**What changed.** `GET /api/v1/admin/connectors` already masked
secret-named keys (`password`, `token`, `api_key`, …) with `******`. It now also strips **userinfo from URL-shaped values at any depth**, so `https://elastic:hunter2@es:9200` comes back as `https://elastic:******@es:9200`. This is what finally covers `url` and `brokers[]`. A credential-free URL is still shown in full — masking it wholesale would hide connector endpoints from the admin UI for no security gain.

**Query parameters with secret-looking names are masked too.** `?api_key=…`,
`?sig=…` and `?X-Amz-Signature=…` used to round-trip in the clear inside a URL value. The parameter name is now judged by the same predicate as an object key. That predicate gained `bearer`, `dsn` and `webhook` (substring matches) plus `pat` and `sig` (exact matches).

**What to do — nothing, but know the round-trip rules.** `update` replaces `config_json` wholesale rather than merging. A `GET` → edit → `PUT` therefore sends masked values back. Each masked position is restored from the stored row *independently*: a masked field, the userinfo password, each secret-named query value. Rotating one in-URL secret while returning the other still masked therefore does the right thing. A mask with no stored counterpart is refused with `400` naming the field, rather than silently overwriting a credential. So is a literal `******` sent under a non-secret query parameter name, since masking can never produce one there. Omit the `config` field from the `PUT` body entirely if you do not intend to change it.

One credential shape is still shown in the clear: a token embedded in a URL *path*, under a generic key such as `url`. A Slack-style webhook is the usual case. A path segment carries no name to judge. Store it under a secret-looking key (`webhook_url`) and the key-name rule masks the whole value.

### Unimplemented secret schemes are rejected

**What changed.** `vault://`, `aws-sm://`, `gcp-sm://` and `azure-kv://` in connector configs were never resolved. The reference string was passed through and **used as the literal password**. Those four schemes are now rejected at connector load. `env://` still works, and ordinary URLs (`postgres://`, `redis://`, `https://`) are untouched.

**How you'll notice.** The connector is **skipped at load** with an `ERROR` log. The server still boots, and `POST`/`PUT` of such a connector through the admin API still returns `201`/`200`. The connector is absent, and workflows referencing it fail at request time. Grep for:

```
Failed to resolve secret reference in connector config, skipping
```

whose `error=` field reads `connector '<name>' config_json: secret scheme 'vault://' is reserved but not supported in this build; supply the value via env:// or a literal instead`.

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

Replace each with `env://VAR_NAME` and inject the secret through your orchestrator. If such a connector appeared to work before, it was authenticating with the literal string `vault://...` as its password — rotate that credential.

### Non-http schemes are refused by SSRF validation

**What changed.** The SSRF validator accepted any URL scheme and only checked
the resolved addresses; it now rejects anything outside `http`/`https` before any DNS work.

**How you'll notice.** An `http_call` or Elasticsearch egress whose URL uses
another scheme (`gopher://`, `ftp://`, `file://`, …) fails with *"only http and https are allowed"*. No supported configuration produced such URLs, so this should be invisible.

### Connector operation gates now cover every connector type

Additive and fully backward compatible — existing connectors behave exactly as before, since every gate defaults to allowed. If you want the new locks:

```json
{ "type": "cache", "backend": "redis", "url": "redis://…", "operations": { "write": false } }
{ "type": "kafka", "brokers": ["…"], "topic": "t", "operations": { "publish": false } }
{ "type": "http",  "url": "https://partner.example.com/v1", "operations": { "methods": ["GET"] } }
```

The HTTP allow-list is exhaustive once non-empty and matches case-insensitively. A method outside `GET`, `POST`, `PUT`, `PATCH` and `DELETE` is rejected with a `400` when the connector is created or updated. So is a gate key the type does not have. A gated call fails with the same validation error the `db`/`es` gates produce.

One interaction is worth knowing. A `cache` connector's `write` gate covers every write through it, **including a channel dedup store or response cache backed by it**. Gating a shared Redis read-only therefore makes any channel pointing its dedup store at that connector fail to load, rather than silently downgrading. There is no `delete` gate on `cache`: the backend trait has no delete.

### Audit-log queries reject unknown parameters

**What changed.** `GET /api/v1/admin/audit-logs` used to ignore unrecognized
query parameters, so a typo silently returned **unfiltered** results that looked like a successful narrow query. Unknown parameters now return `400`.

**How you'll notice.**

```json
{"error": {"code": "VALIDATION_ERROR",
           "message": "Invalid query string: Failed to deserialize query string: unknown field `resource_types`, expected one of `offset`, `limit`, `action`, `resource_type`, `resource_id`, `principal`, `start_time`, `end_time`"}}
```

**What to do.** The accepted parameters are exactly those eight. The `limit` parameter defaults to 50 and is clamped to `[1, 1000]`, and `offset` defaults to 0. Times accept RFC 3339, `%Y-%m-%dT%H:%M:%S` or `%Y-%m-%d %H:%M:%S`, with `start_time` inclusive and `end_time` exclusive. This strictness applies to this endpoint only — no other route changed.

### Audit log: new actor format, new fields, two new settings

**The `principal` column changes format for authenticated callers.** It was the first eight characters of the presented API key, or of its `sha256:` digest. It is now a derived `key-<16 hex>`: `SHA-256("orion:audit:key-id:v1" ‖ SHA-256(key))` truncated to 8 bytes. Three things that buys you:

- Two keys sharing a prefix are now two actors. Any generator with a fixed
  leader (`orion_sk_…`) previously collapsed every key into one.
- The audit log no longer contains eight literal characters of a live
  credential.
- The id is the same whether a key is configured in plaintext or `sha256:`
  form, so rotating an operator between the two does not rename them in the
  trail.

Hold the config and you can recompute the id for each key you issued and map a row back to it. Nobody else can go in either direction. Rows written before the upgrade keep their old values, so a saved `?principal=` filter matches those and nothing new.

**`details` now carries request context** as a JSON object with three fields. The `request_id` field is the same value as the `x-request-id` header, and the `error.request_id` the client was handed. The `client_ip` field is resolved with the `rate_limit.trusted_proxies` policy, so a forged `X-Forwarded-For` cannot dictate it. That policy now applies even with `rate_limit.enabled = false`, so a proxied deployment records the caller rather than the load balancer. The `user_agent` field is truncated to 256 bytes. Unavailable fields are omitted rather than recorded empty. It previously held `{"request_id": …}` at most.

**Mutations immediately before a restart are now recorded.** The write was a
detached task nothing awaited. A mutation accepted moments before `SIGTERM` was answered `200` and then lost — the row an investigation of a bad deploy most wants. It now goes onto a bounded queue drained at shutdown. Two new settings, both with working defaults:

| Setting | Default | Raise it when |
|---|---|---|
| `audit.max_pending` | `1000` | A bursty admin plane (large `/import` batches) overruns the writer |
| `audit.drain_timeout_secs` | `5` | Shutdown reports abandoned rows on a slow database |

Both are refused at `0`. Anything that still does not make it is logged at `error` and counted in `orion_audit_events_dropped_total{reason}`. The reasons are `queue_full`, `write_failed`, `drain_timeout` and `writer_stopped`. Alert on that counter existing at all, not on a threshold: any non-zero value is a hole in the audit trail.

**`POST /admin/workflows/{id}/test` now writes an `action: "test"` row.** It
reads as a dry run and is not one: it executes the workflow's tasks against live connectors. If you have an audit-volume alert, expect it to see traffic from this endpoint for the first time.

## Related

- [Upgrade to 1.0.0](./index.md): the checklist, and every other break.
- [Upgrades](../../operate/maintain/upgrades.md): the version-independent procedure.
- [`orion-server preflight`](../../reference/cli/orion-server/preflight.md): the scan that finds the stored ones.
- [Releases](./index.md): what changed in each version.
