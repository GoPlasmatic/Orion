<!-- description: Orion's three response shapes — admin success, data result, and the shared error envelope — with the complete error code and field-error registries. -->
<!-- type: reference -->
<!-- last_verified: 2026-09-19 -->

# Errors and response envelopes

Every Orion response is one of three JSON shapes: the admin success envelope, the data-plane result envelope, or the shared error envelope. This page owns the error envelope, the data-plane result shapes, and the complete `error.code` registry. [Troubleshooting](../operate/maintain/troubleshooting.md) is the symptom-first guide to the failures these codes name.

## The admin envelope

Every admin 2xx body puts its payload under a top-level `data` key. List endpoints add pagination counters beside it: `limit` and `offset` always, and `total` where the endpoint computes it. The trace list makes `total` opt-in through `?include_total=true` and adds `next_cursor` (see [Data API](./data-api.md)). [Admin API](./admin-api/index.md) documents each endpoint's payload.

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

Three shapes never appear here. Ingress rejections (auth, rate limiting, `validation_logic`, a deduplication replay, backpressure) answer with the [error envelope](#the-error-envelope) and a registry code, such as `429 RATE_LIMITED` or `409 CONFLICT`. A [shaped channel](./data-api.md#shaped-responses) replaces the envelope entirely with a workflow-controlled status, headers and body. And a channel declaring [`response.error_bodies`](./channel-config/response.md#error-bodies) replaces the bytes of a guard rejection with its own template. The status, the code and the error-owned headers stay exactly as the platform set them; only the body changes.

### The async acknowledgment

An `/async` submission answers `202` with an acknowledgment, never a result:

```json
{
  "trace_id": "550e8400-e29b-41d4-a716-446655440000",
  "trace_token": "b1946ac92492d2347c6235b4d2611184"
}
```

Both fields are always present. The trace row is written before the response is sent, so the id can always be polled. The token appears only in this response; Orion stores its hash. Poll `GET /api/v2/admin/traces/{trace_id}` with the token in `x-trace-token`, or with an admin credential. The `?token=` query parameter also works and is [deprecated](./data-api.md#the-token-query-parameter-is-deprecated).

A failed async run surfaces on [the trace](./data-api.md#the-trace-object), not in any HTTP response. Its `status` becomes `failed`, and the trace detail carries the failure text in `error`; list rows name the same value `error_message`. Orion also enqueues the failed delivery to the [trace DLQ](./admin-api/trace-dlq.md) for retry.

A submission shed at the queue answers `503 SERVICE_UNAVAILABLE` with the error envelope. Its already-written trace row is settled as `failed` so no phantom `pending` row remains.

## The code registry

Every general `error.code` the server emits, on both the admin and data planes. The v2 admin API adds [its own registry](#the-v2-admin-code-registry):

<div class="table-filter" data-label="Filter error codes"></div>

| Code | HTTP Status | Description |
|------|-------------|-------------|
| `VALIDATION_ERROR` | 400 | Invalid input — malformed body, failed strict validation, bad query parameter |
| `UNAUTHORIZED` | 401 | Missing or invalid credentials |
| `FORBIDDEN` | 403 | Access denied — a read-only admin key on a mutating method, an `Origin` outside a channel's `origin_allow_list`, or a `channel_call` relaying a target channel's 403. A channel **auth** failure is `UNAUTHORIZED` above |
| `NOT_FOUND` | 404 | Resource not found |
| `METHOD_NOT_ALLOWED` | 405 | The path exists but not for this HTTP method |
| `CONFLICT` | 409 | Duplicate or conflicting state: a database unique/foreign-key constraint a workflow did not catch (see [integrity violations](./workflows.md#integrity-violations)). The v2 admin API's conflicts have codes of their own: see [the v2 admin code registry](#the-v2-admin-code-registry) |
| `PAYLOAD_TOO_LARGE` | 413 | The request body exceeded `ingest.max_payload_size` (data plane) or `server.max_admin_body_size` (admin plane) — the caller's to fix, unlike `RESPONSE_TOO_LARGE` |
| `UNSUPPORTED_MEDIA_TYPE` | 415 | Invalid content type |
| `RATE_LIMITED` | 429 | The caller exceeded a rate limit. The response carries a `Retry-After` header |
| `INTERNAL_ERROR` | 500 | Internal server error |
| `ENGINE_ERROR` | 500 | Workflow execution failed inside the engine for a reason the server does not surface (the detail is in the server log) |
| `STORAGE_ERROR` | 500 | A database operation failed (detail in the server log) |
| `SERIALIZATION_ERROR` | 500 | A stored row could not be decoded or a response could not be serialized — a server-side fault, never client input (detail in the server log) |
| `CONFIG_ERROR` | 500 | A configuration problem surfaced at request time (detail in the server log) |
| `RESPONSE_TOO_LARGE` | 500 | The workflow's result exceeded `trace_queue.max_result_size_bytes` — the request cannot succeed until that cap or the result changes |
| `SERVICE_UNAVAILABLE` | 503 | Backpressure shed the request, a guard's backend failed closed, a quarantined channel was addressed by name, or the service is shutting down |
| `CIRCUIT_OPEN` | 503 | The target connector's circuit breaker is open; retry after it recovers |
| `LOOP_DETECTED` | 508 | A `channel_call` chain came back to a channel already on it, or went deeper than the maximum call depth. A configuration loop: not retryable |
| `TIMEOUT` | 504 | Workflow execution exceeded the channel's timeout |

These codes are stable contract: clients branch on them, and renaming one is a breaking API change.

## The v2 admin code registry

The v2 admin API's own refusals — connector packages, connections, plans and deployments — each carry a code from a closed registry, `orion_api::V2Code`. Every code has one kind, and the kind decides the status: `400` invalid, `403` forbidden, `404` not found, `409` conflict, `500` internal, `503` unavailable. Nothing else is answered under an unregistered code: a finding raised by another layer under a code of its own is answered as `PACKAGE_INVALID`, `ADMISSION_REJECTED` or `DEPLOYMENT_WOULD_NOT_BUILD`, with its code in the message. The v2 endpoints also answer the general codes above — `VALIDATION_ERROR` with field details for a body or a definition that would not serve, `UNAUTHORIZED`, `PAYLOAD_TOO_LARGE`.

<div class="table-filter" data-label="Filter v2 admin codes"></div>

| Code | HTTP Status | Description |
|------|-------------|-------------|
| `PACKAGE_NOT_FOUND` | 404 | No connector package is stored under the digest |
| `ADMISSION_NOT_FOUND` | 404 | The package was never admitted, so there is nothing to revoke |
| `BASE_REVISION_NOT_FOUND` | 404 | The plan or apply names a `base_revision` that was never committed |
| `ROLLBACK_TARGET_NOT_FOUND` | 404 | `rollback_to` names a revision that was never committed |
| `CONNECTION_REVISION_NOT_FOUND` | 404 | The connection has no such revision, or a manifest names one that is not stored |
| `REVISION_NOT_FOUND` | 404 | No committed deployment revision has that number |
| `PROVIDER_NOT_FOUND` | 404 | This node registers no backend for the connection's provider package |
| `PUBLISHER_VERSION_CONFLICT` | 409 | A package with this publisher, provider id and version is stored with other content |
| `IMMUTABLE_CONTENT_CONFLICT` | 409 | An immutable artifact is stored under this identity with other content |
| `IMMUTABLE_CONNECTION_CONFLICT` | 409 | The connection revision is stored with other content; a change is the next revision |
| `IDEMPOTENCY_KEY_CONFLICT` | 409 | The principal used this `idempotency_key` for a different request |
| `DEPLOYMENT_HEAD_CONFLICT` | 409 | The head moved since `base_revision`: another writer committed first. Plan again on the new head |
| `REVIEWED_DIGEST_MISMATCH` | 409 | The manifest no longer plans to the `reviewed_manifest_digest` the caller reviewed; nothing is committed |
| `CANDIDATE_SUPERSEDED` | 409 | A newer revision superseded the one a node was building |
| `PROVIDER_NOT_ADMITTED` | 403 | A manifest names a provider package that is not admitted |
| `PROVIDER_REVOKED` | 403 | The provider package is revoked |
| `ADMISSION_INVALIDATED` | 403 | The package's admission was recorded under another policy or runtime, and no longer holds |
| `ENTERPRISE_TIER_NOT_ENABLED` | 403 | An Enterprise-tier package on a Community node |
| `PACKAGE_INVALID_PROVIDER_ID` | 403 | The admission policy refuses the package's provider id |
| `PACKAGE_INVALID_TARGET` | 403 | The admission policy refuses the package's target |
| `ADMISSION_REJECTED` | 403 | The edition's admission policy refused the package for a reason of its own; the message carries the policy's code |
| `DB_TRANSACTION_ERROR` | 503 | The state database could not begin a transaction (detail in the server log) |
| `DB_WRITE_ERROR` | 503 | The state database could not be written (detail in the server log) |
| `DB_COMMIT_ERROR` | 503 | The state database could not commit (detail in the server log) |
| `DB_READ_ERROR` | 503 | The state database could not be read (detail in the server log) |
| `CORRUPT_PERSISTED_STATE` | 500 | A stored row could not be read back (detail in the server log) |
| `SERIALIZE_ERROR` | 500 | A row the store wrote could not be serialized (detail in the server log) |
| `REQUEST_HASH_INVALID` | 500 | The request's hash could not be computed (detail in the server log) |
| `CANDIDATE_REVISION_MISSING` | 500 | A node was asked to build a revision the store does not hold |
| `PROBE_FAILED` | 500 | A health probe's run could not be started (detail in the server log) |
| `ARCHIVE_ENTRY_COUNT_EXCEEDED` | 400 | The package archive: entry count exceeded |
| `ARCHIVE_SYMLINK_DENIED` | 400 | The package archive: symlink denied |
| `ARCHIVE_DUPLICATE_ENTRY` | 400 | The package archive: duplicate entry |
| `ARCHIVE_COMPRESSION_RATIO_EXCEEDED` | 400 | The package archive: compression ratio exceeded |
| `ARCHIVE_SIZE_LIMIT_EXCEEDED` | 400 | The package archive: size limit exceeded |
| `ARCHIVE_INVENTORY_MISSING` | 400 | The package archive: inventory missing |
| `ARCHIVE_INVENTORY_INVALID` | 400 | The package archive: inventory invalid |
| `ARCHIVE_PATH_INVALID` | 400 | The package archive: path invalid |
| `INVALID_PACKAGE_ENCODING` | 400 | A file in an upload is neither UTF-8 text nor valid `base64:` |
| `PACK_VALIDATION_FAILED` | 400 | `POST /authoring/pack` could not pack the files; the message names the finding |
| `PACKAGE_INVALID` | 400 | A package finding with no code of its own here; the message carries the finding's |
| `INVENTORY_CONTENT_MISMATCH` | 400 | The package inventory: content mismatch |
| `INVENTORY_LIMIT_EXCEEDED` | 400 | The package inventory: limit exceeded |
| `INVENTORY_NOT_CANONICAL` | 400 | The package inventory: not canonical |
| `INVENTORY_ORDER_INVALID` | 400 | The package inventory: order invalid |
| `INVENTORY_PATH_COLLISION` | 400 | The package inventory: path collision |
| `INVENTORY_PATH_INVALID` | 400 | The package inventory: path invalid |
| `INVENTORY_PAYLOAD_MISMATCH` | 400 | The package inventory: payload mismatch |
| `INVENTORY_REQUIRED_FILE_MISSING` | 400 | The package inventory: required file missing |
| `INVENTORY_SELF_REFERENCE` | 400 | The package inventory: self reference |
| `INVENTORY_SHAPE_INVALID` | 400 | The package inventory: shape invalid |
| `CANONICAL_VALUE_UNSUPPORTED` | 400 | A value canonical JSON cannot carry, such as a non-integer number |
| `MANIFEST_INVALID_JSON` | 400 | The package's `connector.json`: invalid json |
| `MANIFEST_SHAPE_INVALID` | 400 | The package's `connector.json`: shape invalid |
| `MANIFEST_ARTIFACT_MISSING` | 400 | The package's `connector.json`: artifact missing |
| `MANIFEST_DEPENDENCY_ALIAS_DUPLICATE` | 400 | The package's `connector.json`: dependency alias duplicate |
| `MANIFEST_ENTRY_MISMATCH` | 400 | The package's `connector.json`: entry mismatch |
| `MANIFEST_IMPORT_PROFILE_CAPABILITY_MISSING` | 400 | The package's `connector.json`: import profile capability missing |
| `MANIFEST_LIMIT_EXCEEDS_PROFILE` | 400 | The package's `connector.json`: limit exceeds profile |
| `MANIFEST_OPERATION_RESERVED` | 400 | The package's `connector.json`: operation reserved |
| `MANIFEST_PATH_INVALID` | 400 | The package's `connector.json`: path invalid |
| `MANIFEST_RETRY_UNSAFE` | 400 | The package's `connector.json`: retry unsafe |
| `MANIFEST_SCHEMA_VERSION_UNSUPPORTED` | 400 | The package's `connector.json`: schema version unsupported |
| `DISTRIBUTION_INVALID_JSON` | 400 | `distribution.json`: invalid json |
| `DISTRIBUTION_SHAPE_INVALID` | 400 | `distribution.json`: shape invalid |
| `DOCUMENT_INVALID_JSON` | 400 | A document in the package: invalid json |
| `DOCUMENT_LIMIT_EXCEEDED` | 400 | A document in the package: limit exceeded |
| `SCHEMA_INVALID_JSON` | 400 | A schema in the package: invalid json |
| `SCHEMA_INVALID` | 400 | A schema in the package: invalid |
| `SCHEMA_PROFILE_LIMIT` | 400 | A schema in the package: profile limit |
| `SCHEMA_REFERENCE_CYCLE` | 400 | A schema in the package: reference cycle |
| `SCHEMA_REFERENCE_DENIED` | 400 | A schema in the package: reference denied |
| `SCHEMA_REFERENCE_MISSING` | 400 | A schema in the package: reference missing |
| `SCHEMA_REFERENCE_NOT_SCHEMA` | 400 | A schema in the package: reference not schema |
| `SCHEMA_UNSUPPORTED_KEYWORD` | 400 | A schema in the package: unsupported keyword |
| `PACKAGE_INVALID_TIER` | 400 | `distribution.json` names a tier that is neither `community` nor `enterprise` |
| `ARTIFACT_NOT_ACTIVE` | 400 | A manifest names an artifact the collector drained; upload it again |
| `ARTIFACT_DRAINING_OR_DELETED` | 400 | The artifact is draining or deleted |
| `LOCK_ALIAS_DUPLICATE` | 400 | The dependency lock: alias duplicate |
| `LOCK_ARTIFACT_UNAVAILABLE` | 400 | The dependency lock: artifact unavailable |
| `LOCK_BINDING_SLOT_MISMATCH` | 400 | The dependency lock: binding slot mismatch |
| `LOCK_CLOSURE_MISSING` | 400 | The dependency lock: closure missing |
| `LOCK_CYCLE` | 400 | The dependency lock: cycle |
| `LOCK_DECLARATION_MISMATCH` | 400 | The dependency lock: declaration mismatch |
| `LOCK_DUPLICATE_NODE` | 400 | The dependency lock: duplicate node |
| `LOCK_GRANT_MISMATCH` | 400 | The dependency lock: grant mismatch |
| `LOCK_IDENTITY_CONFLICT` | 400 | The dependency lock: identity conflict |
| `LOCK_METADATA_MISMATCH` | 400 | The dependency lock: metadata mismatch |
| `LOCK_ORDER_INVALID` | 400 | The dependency lock: order invalid |
| `LOCK_SHAPE_INVALID` | 400 | The dependency lock: shape invalid |
| `LOCK_UNREACHABLE_NODE` | 400 | The dependency lock: unreachable node |
| `LOCK_VERSION_REQUIREMENT_UNQUALIFIED` | 400 | The dependency lock: version requirement unqualified |
| `CONNECTION_REVISION_INVALID` | 400 | The connection revision is malformed (revision 0, an empty id) |
| `CONNECTION_PUBLIC_SECRET_FORBIDDEN` | 400 | A credential sits in `public_config`; put it behind a `secret_locators` reference |
| `CONNECTION_SECRET_LOCATOR_INVALID` | 400 | A secret locator is not a reference (`env://`, `vault://`, …) |
| `CONNECTION_DIGEST_MISMATCH` | 400 | A member's `definition_digest` is not the connection revision's configuration digest |
| `CONNECTION_PROVIDER_NOT_DECLARED` | 400 | A connection's provider is neither declared in `manifest.providers` nor built in |
| `PROVIDER_METADATA_MISMATCH` | 400 | A `manifest.providers` entry's publisher, id or version is not the stored package's |
| `MEMBER_DEFINITION_NOT_FOUND` | 400 | A manifest member names a definition digest that is not stored |
| `OWNING_PACKAGE_NOT_DECLARED` | 400 | A member's `owning_package` is not declared in `manifest.providers` |
| `MEMBER_ARTIFACT_NOT_IN_PACKAGE` | 400 | A member names an artifact its owning package does not carry |
| `ROLLBACK_TARGET_COMPACTED` | 400 | The collector compacted the target revision's members; it can no longer be rolled back to |
| `ENTITY_REF_INVALID` | 400 | A member identity is malformed: an empty id, an id that is a digest, or revision 0 |
| `PROVIDER_REF_INVALID` | 400 | A `manifest.providers` entry is malformed |
| `ROLLOUT_PARTITION_INVALID` | 400 | A member's `rollouts` do not partition the traffic |
| `MANIFEST_VERSION_UNSUPPORTED` | 400 | The deployment manifest: a `schema_version` this server does not read |
| `MANIFEST_DUPLICATE_PROVIDER` | 400 | The deployment manifest: duplicate provider |
| `MANIFEST_OWNER_MISSING` | 400 | The deployment manifest: owner missing |
| `MANIFEST_OWNERSHIP_CONFLICT` | 400 | The deployment manifest: ownership conflict |
| `MANIFEST_DUPLICATE_SINGLETON` | 400 | The deployment manifest: duplicate singleton |
| `MANIFEST_DUPLICATE_MEMBER` | 400 | The deployment manifest: duplicate member |
| `MANIFEST_MISSING_REQUIREMENT` | 400 | The deployment manifest: missing requirement |
| `MANIFEST_CANONICAL_ERROR` | 400 | The deployment manifest cannot be canonicalised |
| `INVALID_MANIFEST_JSON` | 400 | The deployment manifest is not a manifest |
| `DEPLOYMENT_WOULD_NOT_BUILD` | 400 | This node could not build the candidate; nothing is committed |
| `ENGINE_RELOAD_FAILED` | 400 | The node could not rebuild its generation from the candidate |

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
        "expected": ["rest", "http", "cron"], "got": "grpc" }
    ]
  }
}
```

| Field | Type | Present | Description |
|---|---|---|---|
| `path` | string | always | Pointer to the failing key. Rooted two ways; see [How `path` is rooted](#how-path-is-rooted). |
| `code` | string | always | Stable machine-readable identifier from the closed vocabulary below. |
| `message` | string | always | What is wrong with the field. |
| `expected` | any | when known | The accepted value, list, or type. |
| `got` | any | when known | The value that was rejected. |

### Field error codes

The complete vocabulary. It is closed: a code outside this table is a bug. A drift test fails the build if `src/` emits one, or if this table and the `orion-api` registry disagree.

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
| `UNCOMPILED_SOURCE` | The document still carries an authoring convenience a definition set resolves at compile time — a `$from` shared value, a `use` task fragment, a `$use` value fragment, an `$each` repetition, a `$sql` file reference. This endpoint takes one document and has no set to resolve names against; send what `orion-server compile <dir>` writes. See [Shared definitions](./cli/shared-definitions.md). |
| `UNRESOLVED_SECRET_REF` | A secret reference (`env://NAME`, `vault://…`) sits in a workflow field that does not resolve one, so it would be sent on as that literal text — a URL spelled `env://API_BASE` is requested verbatim. Five fields resolve a reference: `crypto.key`, `jwt_sign.key`, and `jwt_verify`'s `keys`, `issuer` and `audience`. See [Environment Variables](./environment-variables.md#where-a-reference-resolves). |

### How `path` is rooted

`path` points at the offending field, rooted according to how far the request got before it was rejected:

- **Validation ran**: the path is resource-rooted and may be indexed: `channel.protocol`, `tasks[2].function.input.connector`. Inside a [task group](./workflows.md#task-groups) the index nests, naming the coordinate as authored rather than the position the task ends up at once the engine flattens the tree: `tasks[1].tasks[0].id`.
- **The body did not deserialize**: validation never ran, and the layer that reports the failure knows the field name but not which resource was being parsed. The path is `body.<field>`, or bare `body` when the field cannot be recovered from the parser's message at all.

The same mistake can surface as `body.protocol` or `channel.protocol` depending on whether it was a parse failure or a validation failure. Match on the last segment to treat both alike.

The same envelope is returned by `POST /api/v2/admin/definitions`, `POST /api/v2/admin/definitions/test`, and the `orion-server lint` / `dry-run` CLI subcommands.

## Validation warnings

`orion-server lint` reports warnings beside its errors. Only errors refuse a workflow, so a workflow can be valid and still carry warnings. These are reported:

| Warning | Meaning |
|---|---|
| `reads '…', which no earlier task writes` | A `data.*` path read by a task that no earlier task writes. |
| `… is JSONLogic in a field nothing evaluates` | An operator node in a connector-payload field, which folds `{"var": …}` and nothing else — so the expression is stored or sent verbatim. |
| `… emits '…' with one '$' stripped` | A `$`-prefixed key in a template position. The engine strips one `$` from every such key, so a `{"$set": …}` update document composed in a `map` goes out as `{"set": …}`. Double the prefix (`$$set`) to emit the `$`. |
| `a failing rule here records status 400 and task '…' still runs` | A [`validation`](./functions/validation.md) whose failure stops nothing: `4xx` warns and carries on, and `on_failure` governs `5xx` only. Add [`"on_rejection": "halt"`](./workflows.md#halting-on-a-rejection), or gate what follows. |

The last three are the engine's own findings, reported wherever a workflow is checked: `orion-server lint`, `orion-server clippy` and the definition-set check. Each carries a stable id there ([`lint`](./cli/orion-server/lint.md)); here they arrive as warnings like any other.

The `reads '…'` warning exists because the failure it predicts is invisible at runtime. JSONLogic resolves an unknown `var` to null. A mistyped path therefore leaves the task running, the workflow succeeding, and the caller receiving a `200` with the field quietly missing.

It is advisory in both directions. Writes are tracked from `parse_json` and `parse_xml` targets, `map` mapping paths, and connector `output` paths. They are matched by prefix: writing `data.order` covers a read of `data.order.total`. Reads of `metadata.*`, `payload`, and the element rebinding inside `map` and `reduce` bodies are out of scope and never warn. A value that legitimately arrives another way, such as a connector response shape or a `continue_on_error` predecessor, can still be flagged. That is why it never blocks a workflow.

## Message sanitization (`verbose_errors`)

[`server.verbose_errors`](./configuration/server.md) decides whether data-plane `errors[]` entries carry the engine's own `message` or this placeholder:

```
Task processing failed; full detail is available in the trace
```

The contract:

- **Only `message` is replaced.** `code` and `task_id` pass through either way.
- **Sanitized is the production posture.** Raw messages can embed upstream URLs, connector names and driver errors, which must not reach anonymous data-plane callers. `verbose_errors = true` is refused in production and the server does not start.
- **Nothing is lost.** The persisted trace keeps the original messages. Correlate with the `request_id` the envelope adds whenever `errors` is non-empty.
- **The data plane only.** Admin 5xx messages always name the failure class; their detail goes to the server log regardless of this setting.
- **Workflow-visible failure records carry no message.** [`metadata._orion_errors`](./workflows.md#branching-on-a-failure) exposes `code`, `task_id`, `workflow_id` and `status` so a workflow can branch on why a step failed, the same fields this contract already lets through. A message there would defeat the setting entirely, because a workflow could copy it into `data`, which is returned unsanitized.

The [configuration reference](./configuration/server.md) owns the setting's values and environment-dependent default.

## Related

- [Troubleshooting](../operate/maintain/troubleshooting.md): the symptom-first guide to the failures these codes name.
- [Admin API](./admin-api/index.md): the endpoints these envelopes wrap, and each payload under `data`.
- [Data API](./data-api.md): routing, traces, shaped responses and profiling around the result envelope.
- [Configuration › Server](./configuration/server.md): the `server.verbose_errors` row and the trace-queue size caps named above.
- [Workflow definition](./workflows.md): the task pipelines whose failures land in `errors[]`.
