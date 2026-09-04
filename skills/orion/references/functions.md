# Task functions

Read this reference when choosing functions or authoring a task's `function.input`.
The running instance is the schema authority:

```bash
orion-cli functions list
orion-cli functions list --quiet
orion-cli functions list --output json \
  | jq '.data[] | select(.name == "data_query")'
```

Do this before writing an unfamiliar input. The catalog reports names,
categories, descriptions, typed fields, expression-capable fields, accepted
secret references, and defaults for the installed release.

## Select the right family

| Need | Prefer |
|---|---|
| Parse ingress | `parse_json`, `parse_xml` |
| Transform or branch | `map`, `filter`, `validation` / `validate` |
| Serialize data | `publish_json`, `publish_xml` |
| Log structured data | `log` |
| HTTP service | `http_call` |
| Portable database access | `data_query`, `data_write` |
| Raw SQL escape hatch | `db_read`, `db_write` |
| Native MongoDB | `mongo_read`, `mongo_write`, `mongo_aggregate` |
| Cache | `cache_read`, `cache_write` |
| Kafka or email | `publish_kafka`, `send_email` |
| Object metadata/presigning | `storage_head`, `storage_presign` |
| Call another Orion service | `channel_call` |
| Cryptography or JWT | `crypto`, `jwt_sign`, `jwt_verify` |

Prefer the portable data dialect unless the operation genuinely requires raw
SQL or native MongoDB. It is parameterized, backend-neutral, and checked against
connector operation gates.

## Task shape

```json
{
  "id": "fetch-customer",
  "name": "Fetch customer",
  "condition": true,
  "continue_on_error": false,
  "function": {
    "name": "data_query",
    "input": {}
  }
}
```

A task condition is strict JSONLogic. Many function input fields accept either
a literal or JSONLogic; the catalog identifies which. A literal remains valid.
Do not invent legacy `*_logic` companions when the schema says the field
itself is expression-capable.

A function's destination is usually `output`, which may itself be an
expression in current releases. `http_call` and `channel_call` retain the
legacy alias `response_path`; supplying both is a duplicate-field error.
A computed destination makes static read/write analysis incomplete, so clippy
will stay conservative.

## Core data functions

Nearly every body-reading workflow begins with:

```json
{
  "name": "parse_json",
  "input": { "source": "payload", "target": "order" }
}
```

This makes the request available at `data.order`. The raw payload is not
available through `{"var":"payload"}`.

`map` applies mappings in order:

```json
{
  "name": "map",
  "input": {
    "mappings": [
      { "path": "data.order.total_with_tax",
        "logic": { "*": [{ "var": "data.order.total" }, 1.18] } }
    ]
  }
}
```

`filter` rejects when its condition is false. `on_reject: "halt"` ends the
workflow; `"skip"` skips that task. A halting filter inside a loop is the
normal early-break mechanism.

`validation` runs rules whose `logic` must be exactly true and reports their
messages without mutating the context.

`publish_json` and `publish_xml` serialize an in-context value. Despite
their names, they do not send data externally.

## Connector functions

A connector function names a connector statically unless its runtime schema
explicitly allows otherwise. Keep connector selection static so activation,
dependency inspection, rename guards, and package closure can prove it exists.

Current `http_call` fields—including headers, path, body, formats, output,
and timeout—can be expressions. Let the connector own the base URL,
authentication, retries, response-size cap, SSRF policy, and allowed methods.
The workflow supplies request-specific values.

`channel_call` runs another channel's workflow in-process. The called channel
keeps its own admission rules, workflow versions, timeout, and metadata. Static
targets appear in `workflows dependencies`; a dynamic target makes that report
explicitly incomplete. Orion detects cycles and enforces a call-depth limit.

Connector errors that are continued are recorded in
`metadata._orion_errors`. Database constraint failures have stable codes such
as `integrity_unique`, `integrity_foreign_key`, `integrity_not_null`, and
`integrity_check`; branch on codes, never backend error text.

## Credentials and environment-specific values

Do not place credentials in ordinary inputs. Use:

- a connector with `env://...` or `vault://...` secret references;
- a declared secret read with `{"secret":"name"}` in expression-safe fields;
- the explicit key-bearing fields of `crypto`, `jwt_sign`, and `jwt_verify`.

Only fields marked by the runtime schema accept direct secret references.
Elsewhere Orion refuses them with `UNRESOLVED_SECRET_REF`. A declared secret
cannot be read where its result would be persisted, such as a map result or log
field.

`crypto` and JWT functions perform no external I/O, so offline dry-runs can
execute them rather than stub them.

## Offline egress behavior

`orion-server dry-run` and `test` do not contact external systems by default.
Provide deterministic stubs for connector functions and `channel_call`.
Use the exact stub syntax reported by the command's `--help` and assert both
the trace and final context. Never turn a workflow test into an accidental live
dependency test.
