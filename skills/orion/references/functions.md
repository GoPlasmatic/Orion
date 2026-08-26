# Task functions

27 names are valid in a task's `function.name`. **Get the authoritative input
schema from the running instance** — this file is orientation, not
specification:

```bash
orion-cli functions list --quiet                              # every valid name
orion-cli functions list --output json | jq '.data[] | select(.name == "http_call")'
```

Two classes behave differently at write time:

- **Engine built-ins** (from dataflow-rs) are *not* input-schema-validated when
  the workflow is created, so a bad `input` surfaces at execution.
- **Connector and utility functions** (Orion's own) *are* validated on
  create/update, and their schemas appear in `functions list`.

| Group | Names |
|---|---|
| Data (engine) | `parse_json`, `parse_xml`, `map`, `filter`, `validation` (alias `validate`), `log`, `publish_json`, `publish_xml` |
| HTTP | `http_call` |
| Portable data | `data_query`, `data_write` |
| Raw SQL | `db_read`, `db_write` |
| MongoDB | `mongo_read`, `mongo_write`, `mongo_aggregate` |
| Cache | `cache_read`, `cache_write` |
| Messaging | `publish_kafka`, `send_email` |
| Object storage | `storage_presign`, `storage_head` |
| Composition | `channel_call` |
| Utility | `crypto`, `jwt_sign`, `jwt_verify` |

Prefer `data_query` / `data_write` over `db_read` / `db_write`: the portable
dialect is parameterized, injection-safe and backend-neutral. The raw pair is
the escape hatch for SQL the dialect cannot express.

`crypto`, `jwt_sign` and `jwt_verify` need no connector and make no egress, so
`orion-server dry-run` executes them for real rather than stubbing them.

## The ones you will use constantly

### `parse_json` / `parse_xml`

Almost every workflow starts here — without it, conditions reading `data.*` see
an empty context.

| Field | Required | Notes |
|---|:--:|---|
| `source` | yes | Where to read the raw value, e.g. `"payload"` |
| `target` | yes | Parsed value is stored at `data.{target}` |

`parse_xml` takes the same two fields.

```json
{ "name": "parse_json", "input": { "source": "payload", "target": "order" } }
```

### `map`

The primary tool for reshaping, computing and enriching. An ordered list of
JSONLogic expressions, each written to a dotted path.

| Field | Required | Notes |
|---|:--:|---|
| `mappings` | yes | Ordered `{ "path", "logic" }` entries |
| `mappings[].path` | yes | Dotted target, e.g. `"data.order.total"` |
| `mappings[].logic` | yes | Expression whose result is written there |

```json
{ "name": "map", "input": { "mappings": [
  { "path": "data.order.flagged", "logic": true },
  { "path": "data.order.total_with_tax",
    "logic": { "*": [{ "var": "data.order.total" }, 1.1] } }
] } }
```

A misspelled operator here is **silent** — see `expressions.md`.

### `filter`

| Field | Required | Default | Notes |
|---|:--:|---|---|
| `condition` | yes | — | Evaluated against the data context |
| `on_reject` | no | `"halt"` | `"halt"` stops the workflow; `"skip"` skips only this task |

`on_reject: "halt"` inside a `loop` body ends the whole loop — that is the
idiomatic early break.

### `validation` / `validate`

Each rule's `logic` must evaluate to exactly `true`; anything else records the
`message`. Non-destructive — it never mutates the context.

```json
{ "name": "validation", "input": { "rules": [
  { "logic": { "!!": [{ "var": "data.order.customer_id" }] }, "message": "customer_id is required" },
  { "logic": { ">": [{ "var": "data.order.total" }, 0] }, "message": "total must be positive" }
] } }
```

### `log`

| Field | Required | Default | Notes |
|---|:--:|---|---|
| `message` | yes | — | JSONLogic; a plain string is valid |
| `level` | no | `"info"` | `trace` \| `debug` \| `info` \| `warn` \| `error` |
| `fields` | no | `{}` | name → JSONLogic, logged as structured fields |

### `publish_json` / `publish_xml`

These serialize a field **inside** the context to a string at another field.
They do **not** publish to an external system — the name misleads.

| Field | Required | Default | Notes |
|---|:--:|---|---|
| `source` | yes | — | Field under `data`, e.g. `"order"` reads `data.order` |
| `target` | yes | — | Field under `data` receiving the string |
| `pretty` | no | `false` | `publish_json` only |
| `root_element` | no | `"root"` | `publish_xml` only |

### `http_call`

The connector supplies the base URL and auth; the workflow never holds
credentials.

| Field | Required | Default | Notes |
|---|:--:|---|---|
| `connector` | yes | — | Name of an HTTP connector |
| `method` | no | `"GET"` | `GET` \| `POST` \| `PUT` \| `PATCH` \| `DELETE` |
| `path` / `path_logic` | no | — | Appended to the connector's base URL; `_logic` computes it |
| `headers` | no | `{}` | Extra headers, string → string |
| `body` / `body_logic` | no | — | Static or computed request body |
| `body_format` | no | `"json"` | `json` \| `form` \| `text` |
| `output` | no | — | Dotted path for the response; omit to discard |
| `response_format` | no | `"json"` | `json` (parsed, fails if invalid) \| `text` |
| `timeout_ms` | no | `30000` | Per-request timeout |

`body_format: "form"` URL-encodes for OAuth token endpoints and form APIs:
scalars encode directly, arrays become repeated keys, `null` entries are
skipped, nested values are rejected. Each format stamps its own `content-type`;
setting one in `headers` changes the label, never the bytes.

```json
{ "name": "http_call", "input": {
  "connector": "payment-api", "method": "POST", "path": "/charge",
  "body_logic": { "var": "data.payment" },
  "output": "data.charge_result", "timeout_ms": 5000
} }
```

### `channel_call`

Invokes another channel's workflow **in-process** — no network hop. The target
keeps its own versioning and governance; cycle detection and a max call depth
prevent runaway recursion.

| Field | Required | Default | Notes |
|---|:--:|---|---|
| `channel` / `channel_logic` | exactly one | — | Static or computed target channel name |
| `data` / `data_logic` | at most one | request payload | Static or derived payload |
| `output` | no | `"data"` | Dotted path for the response |
| `timeout_ms` | no | from config | Per-call timeout |

A dynamic `channel_logic` makes the static dependency list incomplete —
`orion-cli workflows dependencies <id>` says so when it cannot resolve targets.

## Output paths

Every connector function writes to a dotted path named `output`. `http_call`
and `channel_call` also accept the pre-1.0 spelling `response_path`; supplying
**both** is a duplicate-field error, not a precedence rule.

For the remaining functions — the data dialect, Mongo, cache, Kafka, email,
storage, crypto and JWT — read the schema off the instance rather than guessing:

```bash
orion-cli functions list --output json | jq '.data[] | select(.name == "data_query")'
```
