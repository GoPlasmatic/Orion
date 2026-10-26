<!-- description: The connector_call task function: invoke a v2 connector provider operation (rust, workflow, or wasm) under an owned RunContext. -->
<!-- type: reference -->
<!-- last_verified: 2026-10-24 -->

# `connector_call`

Invokes a v2 connector provider operation (`rust`, `workflow`, or `wasm`) under an owned `RunContext`, enforcing admission, capability grants, reserved-operation denial, and a normalized `ConnectorError` on failure.

## Synopsis

```json
{
  "name": "connector_call",
  "input": {
    "connection": "orion.foundation.memory-cache",
    "operation": "cache.set",
    "kind": "rust",
    "input": {
      "key": { "var": "data.order_id" },
      "value": { "var": "data.payload" }
    },
    "output": "data.cached"
  }
}
```

## Description

`connector_call` dispatches a public operation on a registered v2 provider backend (`rust`, `workflow`, or `wasm`). Internal lifecycle operations prefixed with `orion.internal.` are forbidden from workflow steps and fail immediately with `CONNECTOR_OPERATION_FORBIDDEN`.

### Which calls a workflow may make

A call runs only in a generation built from a deployment manifest, and only when the workflow's manifest member `requires` the connection. The operations granted are the ones the workflow's own `connector_call` tasks name for it. The connection must be a member of the deployed manifest, and the call uses exactly the revision that member names, never one stored since. Every call of one run shares that run's deadline and budget. Anything else is refused with `CONNECTOR_CONNECTION_FORBIDDEN`: a call outside a served run, a connection the member does not require, a connection the manifest does not deploy, or a provider package that is not uploaded.

A connection revision names its provider by package digest. The built-in drivers (`org.orion.http`, `org.orion.postgresql`, `org.orion.mysql`, `org.orion.sqlite`, `org.orion.memory-cache`) have no uploaded package; each answers to a fixed digest instead, listed as `package_digest` under `provider_backends` in `GET /api/v2/admin/runtime-capabilities`. A connection naming one needs no entry in the manifest's `providers`, and can be called only as that driver: a call whose `target_id` is another driver is refused with `CONNECTOR_CONNECTION_FORBIDDEN`.

`kind` must be `rust`, `workflow` or `wasm`. Any other value is refused when the task loads.

### Workflow provider packages

An uploaded provider package whose `implementation.kind` is `workflow` serves each of its operations with a dataflow workflow, and runs it on this node's engine with Orion's operators. Once the package is admitted it is registered under its digest; revoking it unregisters it, and a node that restarts registers every admitted package again from the store. Call one with `kind: "workflow"`. The `target_id` is the package digest the connection's revision names, so it may be left out:

```json
{
  "name": "connector_call",
  "input": {
    "connection": "crm",
    "kind": "workflow",
    "operation": "item.get",
    "input": { "id": { "var": "data.id" } },
    "output": "data.item"
  }
}
```

The operation's `input` is the workflow's payload and its starting `data`, and the result is `data` when the workflow finishes. A workflow that fails its tasks fails the call with `PROVIDER_WORKFLOW_FAILED`; one past the call's deadline with `CONNECTOR_DEADLINE_EXCEEDED`.

The manifest maps each operation to one entry workflow, `implementation.entries` (`operation → workflow id`), and lists the workflow files under `implementation.workflows`. A package workflow names no `channel`; its operation addresses it. There are no helper workflows and no sub-channels: a workflow that is no operation's entry, or an `implementation` key other than `kind`, `entries` and `workflows`, is refused. Share logic between operations with compile-time fragments.

A package is code someone else may have written, so what its workflows may do is closed. A package workflow may call:

- the dataflow built-ins: `map`, `validation`, `filter`, `log`, `parse_json`, `parse_xml`, `publish_json` and `publish_xml`;
- `crypto`, `jwt_sign` and `jwt_verify`;
- plugin functions, which import nothing;
- `connector_call`, on a dependency alias its manifest declares under `dependencies`, with one of the `operations` that dependency lists, and no `kind` or `target_id`.

Every other function is refused, because each one reaches the deployment's own connections or channels by name: `http_call`, `db_read`, `db_write`, `data_query`, `data_write`, the `cache_*` functions, `cache_invalidate` and `channel_call`. A `{"secret": …}` node or a secret reference such as `env://…` anywhere in a package workflow is refused too. These rules are checked when the package is uploaded, when it is admitted, and again whenever a deployment that serves a connection over it is built. Each refusal is a `400` whose `details` name the file and the path, such as `workflows/entry.json:tasks[2].function.name`. The engine a package runs on holds only the functions above, on an expression engine with no secrets. Its messages carry no `metadata` and no `[vars]`. A document that slipped past the check still could not reach anything else.

A dependency call goes to the connection the package's connection [binds](../admin-api/deployments.md#bindings) the alias to, as a dependency of the package's own invocation: it shares the caller's deadline and budget, and the bound connection is reachable from that package's calls and from nothing else.

### The foundation SQL and HTTP providers

The Community providers `org.orion.postgresql`, `org.orion.mysql`, `org.orion.sqlite` and `org.orion.http` take their target from the connection revision, never from `input`. An `input` carrying `connection_string`, `url` or `base_url` is refused with `CONNECTOR_CONNECTION_FORBIDDEN`. A revision's `public_config` names the target (`connection_string`, or `url` for HTTP) and never holds a credential: a credential-shaped key or value there is refused when the revision is created. `org.orion.mysql` serves MySQL and MariaDB and takes a `mysql://` or `mariadb://` connection string; any other scheme is refused with `CONNECTOR_INVALID_INPUT`.

A credential comes from the revision's `secret_locators`: a map from a config path to a secret reference such as `env://ORDERS_DB_URL` or `vault://secret/orders#password`. Each reference is resolved where it is used and placed at its path, so `{"connection_string": "env://ORDERS_DB_URL"}` supplies a SQL connection's whole connection string. A locator must be a reference, never the secret itself, and may not name a path `public_config` already sets.

For HTTP, `public_config.auth` says how the credential is sent, and its secret field comes from the locator at that field's path:

- `{"type": "bearer"}` with `auth.token` sends `Authorization: Bearer <token>`.
- `{"type": "api_key", "header": "X-API-Key"}` with `auth.key` sends the key in the named header, which may not be `Host`, `Content-Length`, `Content-Type` or `Transfer-Encoding`.
- `{"type": "basic", "username": "svc"}` with `auth.password` sends HTTP Basic.

An `auth` with no locator for its credential is refused with `CONNECTOR_INVALID_INPUT` before any request is made, as is an `auth` with an unknown `type` or a field its type does not take.

A SQL statement (`input.sql`, or `source` for a query and `target` for a write) must be a literal string. A task that builds one from the message, such as `{"sql": {"var": "data.q"}}`, is refused when it loads. The message reaches the database only through `input.params`, an array bound to the statement's placeholders (`$1` on PostgreSQL, `?` on MySQL and SQLite):

```json
{
  "connection": "orders-db",
  "target_id": "org.orion.postgresql",
  "operation": "query",
  "input": {
    "sql": "SELECT id, total FROM orders WHERE customer_id = $1",
    "params": [{ "var": "data.customer_id" }]
  },
  "output": "data.orders"
}
```

A query's result is held to `query.max_limit` rows and `query.max_result_bytes`, counted as rows stream, the same caps [`db_read`](./db_read.md) has. A transport failure, a serialization failure, a deadlock or a MySQL lock wait timeout is `CONNECTOR_UNAVAILABLE` and retryable; any other error the database raises is `CONNECTOR_INVALID_INPUT` and is not. An HTTP response body is capped at 10 MiB.

**Retry safety:** `depends_on` `operation`. See [Retry safety](./retry-safety.md) for what the answer costs.

## Fields

| Field | Type | Required | Default | Description |
|-------|------|:--------:|---------|-------------|
| `connection` | string | yes | — | Logical connection ID or driver identifier |
| `operation` | string | yes | — | Public operation name (e.g. `http.request`, `sql.query`, `cache.get`) |
| `kind` | string | no | `"rust"` | Provider implementation kind (`rust`, `workflow`, or `wasm`) |
| `target_id` | string | no | `connection` | Optional provider target ID (`driver_id` for `rust`, `package_digest` for `workflow`/`wasm`) |
| `input` | object | no | `{}` | Operation input object (`{"var": ...}` references are resolved from the message context) |
| `idempotency_key` | string \| JSONLogic | no | — | Optional idempotency key expression for retry-capable write operations |
| `output` | string \| JSONLogic | no | `"data"` | Dotted context path where the operation result is written |

## Related

- [Task functions](./index.md): the whole catalogue, and each function's retry safety.
- [Retry safety](./retry-safety.md): what a retry of each function costs.
- [Runtime function discovery](./runtime-discovery.md): the live catalogue.
