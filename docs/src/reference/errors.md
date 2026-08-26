# Errors & Response Envelopes

Every Orion response is one of three JSON shapes: the admin success envelope, the data-plane result envelope, or the shared error envelope. This page is the owner of the error envelope, the data-plane result shapes, and the complete `error.code` registry.

## The admin envelope

Every admin 2xx body puts its payload under a top-level `data` key. List endpoints add pagination counters alongside it: `limit` and `offset` always, and `total` where the endpoint computes it — the trace list makes `total` opt-in via `?include_total=true` and adds `next_cursor` (see [Data API](./data-api.md)). The [Admin API](./admin-api.md) documents each endpoint's payload.

### The error envelope

Every non-2xx response, on both planes, carries one structure:

```json
{
  "error": {
    "code": "NOT_FOUND",
    "message": "Workflow with id 'wf_orders' not found",
    "request_id": "9b2d64ec-6f0a-4f6e-9c1a-6d1f2b3c4d5e"
  }
}
```

| Field | Type | Present | Description |
|---|---|---|---|
| `code` | string | always | A value from [the code registry](#the-code-registry). Branch on this field, never on `message`. |
| `message` | string | always | Human-readable detail. The wording is not contract and may change. |
| `details` | array | validation failures only | Field-pathed entries — see [Field-pathed validation details](#field-pathed-validation-details). Omitted when empty. |
| `request_id` | string | when the request has an id | Echoes the `x-request-id` response header. Orion generates the id when the caller sends none, so it is present on real requests. |

A `429` response always carries a `Retry-After` header alongside the envelope.

> [!NOTE]
> A 5xx `message` names the failure class, never the internal cause. The full detail — driver errors, upstream URLs — goes to the server log.

## The data-plane envelope

### The sync result envelope

A sync channel answers `200` with the result envelope, whatever happened inside the workflow:

```json
{
  "id": "6e7f0a4b-8c2d-4e1f-9a3b-5c6d7e8f9a0b",
  "status": "ok",
  "data": { "order": { "order_id": "ORD-123", "flagged": true } },
  "errors": []
}
```

| Field | Type | Description |
|---|---|---|
| `id` | string | Engine message id — the correlation key inside the persisted trace. |
| `status` | string | Always `"ok"`. Task failures are reported in `errors`, never by flipping this field. |
| `data` | object | Workflow output. The shape is entirely channel-defined. |
| `errors` | array | Per-task failures. Empty on a clean run. |
| `request_id` | string | Present only when `errors` is non-empty. Correlates the response with the stored trace. |
| `_orion` | object | Present only when profiling was requested — see [per-request profiling](./data-api.md#per-request-profiling). |

> [!WARNING]
> `200` does not mean the workflow succeeded. A failed task still answers `200` — check `errors` before trusting `data`.

Each `errors[]` entry:

| Field | Type | Description |
|---|---|---|
| `code` | string | The engine's failure code for the task, for example `TASK_FAILED`. These codes are separate from [the code registry](#the-code-registry). |
| `message` | string | The failure text, sanitized by default — see [Message sanitization](#message-sanitization-verbose_errors). |
| `task_id` | string | The failing task. Omitted for workflow-level failures. |

Three shapes never appear here. Ingress rejections — auth, rate limiting, `validation_logic`, a deduplication replay, backpressure — answer with the [error envelope](#the-error-envelope) and a registry code, such as `429 RATE_LIMITED` or `409 CONFLICT`. A [shaped channel](./data-api.md#shaped-responses) replaces the envelope entirely with a workflow-controlled status, headers, and body. And a channel declaring [`response.error_bodies`](./channel-config.md#error-bodies) replaces the *bytes* of a guard rejection with its own template — the status, the code and the error-owned headers stay exactly as the platform set them, and only the body changes.

### The async acknowledgment

An `/async` submission answers `202` with an acknowledgment, never a result:

```json
{
  "trace_id": "550e8400-e29b-41d4-a716-446655440000",
  "trace_token": "b1946ac92492d2347c6235b4d2611184"
}
```

Both fields are always present — the trace row is written before the response is sent, so the id can always be polled. The token appears only in this response; Orion stores its hash. Poll `GET /api/v1/admin/traces/{trace_id}` with the token in `x-trace-token` (or `?token=`), or with an admin credential.

A failed async run surfaces on [the trace](./data-api.md#the-trace-object), not in any HTTP response. Its `status` becomes `failed`, and the trace detail carries the failure text in `error` — list rows name the same value `error_message`. Orion also enqueues the failed delivery to the [trace DLQ](./admin-api.md#trace-dlq) for retry.

A submission shed at the queue answers `503 SERVICE_UNAVAILABLE` with the error envelope. Its already-written trace row is settled as `failed` so no phantom `pending` row remains.

## The code registry

Every `error.code` the server emits, on both the admin and data planes:

<div class="table-filter" data-label="Filter error codes"></div>

| Code | HTTP Status | Description |
|------|-------------|-------------|
| `VALIDATION_ERROR` | 400 | Invalid input — malformed body, failed strict validation, bad query parameter |
| `UNAUTHORIZED` | 401 | Missing or invalid credentials |
| `FORBIDDEN` | 403 | Access denied — a read-only admin key on a mutating method, an `Origin` outside a channel's `origin_allow_list`, or a `channel_call` relaying a target channel's 403. A channel **auth** failure is `UNAUTHORIZED` above |
| `NOT_FOUND` | 404 | Resource not found |
| `METHOD_NOT_ALLOWED` | 405 | The path exists but not for this HTTP method |
| `CONFLICT` | 409 | Duplicate or conflicting state — e.g. a second draft, an import collision, or an idempotency-key replay |
| `PAYLOAD_TOO_LARGE` | 413 | The request body exceeded `ingest.max_payload_size` (data plane) or `server.max_admin_body_size` (admin plane) — the caller's to fix, unlike `RESPONSE_TOO_LARGE` |
| `UNSUPPORTED_MEDIA_TYPE` | 415 | Invalid content type |
| `RATE_LIMITED` | 429 | Too many requests. The response carries a `Retry-After` header |
| `INTERNAL_ERROR` | 500 | Internal server error |
| `ENGINE_ERROR` | 500 | Workflow execution failed inside the engine for a reason the server does not surface (the detail is in the server log) |
| `STORAGE_ERROR` | 500 | A database operation failed (detail in the server log) |
| `SERIALIZATION_ERROR` | 500 | A stored row could not be decoded or a response could not be serialized — a server-side fault, never client input (detail in the server log) |
| `CONFIG_ERROR` | 500 | A configuration problem surfaced at request time (detail in the server log) |
| `RESPONSE_TOO_LARGE` | 500 | The workflow's result exceeded `trace_queue.max_result_size_bytes` — the request cannot succeed until that cap or the result changes |
| `SERVICE_UNAVAILABLE` | 503 | Backpressure shed the request, a guard's backend failed closed, a quarantined channel was addressed by name, or the service is shutting down |
| `CIRCUIT_OPEN` | 503 | The target connector's circuit breaker is open; retry after it recovers |
| `TIMEOUT` | 504 | Workflow execution exceeded the channel's timeout |

These codes are stable contract: clients branch on them, and renaming one is a breaking API change.

## Field-pathed validation details

When a workflow, channel, or connector fails strict validation on create or update, the error envelope gains a `details` array. `details` stays omitted for single-message errors.

```json
{
  "error": {
    "code": "VALIDATION_ERROR",
    "message": "Workflow validation failed",
    "details": [
      { "path": "tasks[0].function.input.connector", "code": "REQUIRED", "message": "is required" },
      { "path": "channel.protocol", "code": "INVALID", "message": "unknown protocol",
        "expected": ["rest", "http", "kafka"], "got": "grpc" }
    ]
  }
}
```

| Field | Type | Present | Description |
|---|---|---|---|
| `path` | string | always | Pointer to the failing key. Rooted two ways — see below. |
| `code` | string | always | Stable machine-readable identifier from the closed vocabulary below. |
| `message` | string | always | What is wrong with the field. |
| `expected` | any | when known | The accepted value, list, or type. |
| `got` | any | when known | The value that was rejected. |

### Field error codes

The complete vocabulary. It is closed: a code outside this table is a bug, and
a drift test fails the build if `src/` emits one or if this table and the
`orion-api` registry disagree.

| Code | Meaning |
|---|---|
| `REQUIRED` | The field was absent and is always required. |
| `REQUIRED_FOR_PROTOCOL` | Required for this `protocol` or `channel_type`, though optional in general — a REST channel without `methods`, say. |
| `INVALID` | Present and well-typed, but not an acceptable value. |
| `TYPE_MISMATCH` | Present but the wrong JSON type. |
| `TOO_LONG` | Longer than the column or protocol allows. |
| `UNKNOWN_FIELD` | A key the strict parser does not accept — a typo, or a pre-1.0 spelling 1.0 refuses. |
| `DUPLICATE_FIELD` | The same key appeared twice in one object. |
| `DUPLICATE_TASK_ID` | Two steps in one workflow declare the same `id`. Tasks and task groups share one id namespace. |
| `UNKNOWN_FUNCTION` | A task names a function the engine does not register — the workflow would be accepted and then fail at its first request. When the name is a plausible typo, the message appends the closest registered name (`did you mean …?`). |

### How `path` is rooted

`path` points at the offending field, rooted according to how far the request
got before it was rejected:

- **Validation ran** — the path is resource-rooted and may be indexed:
  `channel.protocol`, `tasks[2].function.input.connector`. Inside a
  [task group](./workflows.md#task-groups) the index nests, naming the
  coordinate as authored rather than the position the task ends up at once the
  engine flattens the tree: `tasks[1].tasks[0].id`.
- **The body did not deserialize** — validation never ran, and the layer that
  reports the failure knows the field name but not which resource was being
  parsed. The path is `body.<field>`, or bare `body` when the field cannot be
  recovered from the parser's message at all.

So the same mistake can surface as `body.protocol` or `channel.protocol`
depending on whether it was a parse failure or a validation failure. Match on
the last segment if you want to treat both alike.

The same envelope is returned by `POST /workflows/validate`, `POST /workflows/{id}/test`, and the `orion-server lint` / `dry-run` CLI subcommands.

## Validation warnings

`POST /workflows/validate` returns `{ "valid", "errors", "warnings" }`. `valid` reflects `errors` only — it means "`POST /workflows` would accept this" — so a workflow can be valid and still carry warnings. Two are reported:

| Warning | Meaning |
|---|---|
| `Connector '…' not found in registry` | The task names a connector that does not exist yet. Not an error at create time (connectors and workflows may be authored in either order), but **activation refuses it**. |
| `reads '…', which no earlier task writes` | A `data.*` path read by a task that no earlier task writes. |

The second warning exists because the failure it predicts is invisible at runtime. JSONLogic resolves an unknown `var` to null. A mistyped path therefore leaves the task running, the workflow succeeding, and the caller receiving a `200` with the field quietly missing.

It is advisory in both directions. Writes are tracked from `parse_json`/`parse_xml` targets, `map` mapping paths, and connector `output` paths, and matched by prefix — writing `data.order` covers a read of `data.order.total`. Reads of `metadata.*`, `payload`, and the element rebinding inside `map`/`reduce` bodies are out of scope and never warn. A value that legitimately arrives another way — a connector response shape, a `continue_on_error` predecessor — can still be flagged, which is why it never blocks creation.

## Message sanitization (`verbose_errors`)

[`server.verbose_errors`](./configuration.md#server) decides whether data-plane `errors[]` entries carry the engine's own `message` or this placeholder:

```
Task processing failed; full detail is available in the trace
```

The contract:

- **Only `message` is replaced.** `code` and `task_id` pass through either way.
- **Sanitized is the production posture.** Raw messages can embed upstream URLs, connector names, and driver errors, which must not reach anonymous data-plane callers. `verbose_errors = true` is refused in production and the server will not start.
- **Nothing is lost.** The persisted trace keeps the original messages. Correlate with the `request_id` the envelope adds whenever `errors` is non-empty.
- **The data plane only.** Admin 5xx messages always name the failure class; their detail goes to the server log regardless of this setting.
- **Workflow-visible failure records carry no message.** [`metadata._orion_errors`](./workflows.md#branching-on-a-failure) exposes `code`, `task_id`, `workflow_id` and `status` so a workflow can branch on *why* a step failed — the same fields this contract already lets through. A message there would defeat the setting entirely: a workflow could copy it into `data`, which is returned unsanitized.

The [configuration reference](./configuration.md#server) owns the setting's values and environment-dependent default.

## Related

- [Admin API](./admin-api.md) — the endpoints these envelopes wrap, and each payload under `data`.
- [Data API](./data-api.md) — routing, traces, shaped responses, and profiling around the result envelope.
- [Configuration](./configuration.md#server) — the `server.verbose_errors` row and the trace-queue size caps named above.
- [Workflows](./workflows.md) — the task pipelines whose failures land in `errors[]`.
