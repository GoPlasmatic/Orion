# Test Workflows Offline

`orion-server` runs workflows without a server, a database, or a network, so a
workflow can be developed and regression-tested the way any other code is. This
page is the CI author's reference for those commands.

Four gates, cheapest first: `lint`, `dry-run`, `test`, and `test-connectivity`.
The first three need nothing but the binary and your JSON files.

## Lint a workflow file

```bash
orion-server lint workflow.json      # one file
orion-server lint ./definitions      # the whole set, and the references between files
```

```
'workflow.json' is valid.
```

Pointing it at a **directory** validates every channel, workflow and connector under it *and* resolves the references between them — a `channel_call` target that exists nowhere, a task naming a connector of the wrong type, two channels claiming one route. Those cannot be caught one file at a time, because the file that would disprove them is one the command never opens. See the [CLI reference](../reference/cli.md#lint) for the flags.

`lint` applies the same validators the admin API applies on create: task shapes,
function names, and each connector function's `input` schema. It exits non-zero
on any finding, which is all a pull-request gate needs.

It also prints **advisory warnings** on stderr without failing — today, one:
JSONLogic in a connector field that folds `{"var": …}` and nothing else, so the
expression is stored or sent verbatim. `--deny-warnings` turns those into a
failure for a gate that wants them to block:

```bash
orion-server lint workflow.json --deny-warnings
```

It stays advisory by default because operator names (`length`, `type`, `keys`,
`in`) are ordinary field names, and a document that legitimately holds a stored
rule is a real payload. `POST /api/v1/admin/workflows/validate` reports the same
findings in its `warnings` array.

Every `orion-server` subcommand run without `-c` prints
`Note: no config file specified…` on **stderr**. It is not a finding; redirect
it or pass a config file.

## Dry-run against sample input

```bash
orion-server dry-run -w workflow.json -i payload.json
```

`-i` takes the **bare payload** — not the `{"data": …}` envelope the HTTP API
uses. `--metadata` takes a second file holding the request metadata the HTTP
ingress would have built — see [Supply request metadata](#supply-request-metadata).
The command prints one JSON document on stdout, so `jq` can read it:

| Field | Holds |
|---|---|
| `matched` | Whether any task ran |
| `trace` | The per-task execution path, including which tasks were skipped |
| `output` | The final data document, under its historical name |
| `data` | The same document, under the name a case's `expect` roots use |
| `metadata` | The final metadata document |
| `temp_data` | The final scratch document |
| `audit_trail` | One entry per executed task, with its writes |
| `calls` | Connector calls grouped by function, each with its resolved payload |
| `errors` | Task errors, if any |

The five documents after `trace` are the same set, in the same shape, that a
case's `expect` roots address — so a path read off a dry run works in a case
unchanged. `output` is kept as an alias of `data` because CI `jq` filters read
it. `calls` is grouped by function rather than flat; each record carries a
`seq` if you need the order across functions.

```bash
orion-server dry-run -w workflow.json -i payload.json | jq '.output.order'
```

It exits non-zero when the run fails, and prints the trace either way — a run
that dies at task three still tells you what the first two did.

## Stub the calls that leave the process

Connector-backed tasks — `http_call`, `db_read`, `data_query`, `channel_call`,
and the rest — are answered from a **stub file** rather than a real backend:

```json
{
  "http_call":    { "crm": { "name": "Ada Lovelace" } },
  "data_query":   { "orders-db": [ { "id": 1, "total": 10 } ] },
  "channel_call": { "inventory-check": { "in_stock": true } },
  "db_write":     { "*": { "rows_affected": 1 } }
}
```

The outer key is the function name, the inner key is the task's `connector` — or
its `channel` for `channel_call` — and `"*"` matches any target. The value is
what the task writes to its `output` path.

```bash
orion-server dry-run -w workflow.json -i payload.json --stubs stubs.json
```

> [!IMPORTANT]
> **Stubbing is all-or-nothing.** A task with no matching stub **fails**, and
> the error names the stub that would satisfy it. A half-stubbed run reporting
> success would be worse than no stubs at all, because it looks like a pass.

Two mistakes are caught when the file is parsed rather than at the failing task:
naming a function that is not connector-backed, and putting the response where
the target map belongs. Both would otherwise produce a file that parses fine and
matches nothing.

> [!NOTE]
> This is the offline counterpart to `POST /workflows/{id}/test`, which runs the
> same workflow against **live** connectors — real webhooks, real databases,
> real topics. Reach for the endpoint when you mean to touch the real systems,
> and for `dry-run` when you do not.

## Build a regression suite

A case is any `*.case.json` file — the suffix is what separates cases from the
workflows and fixtures beside them:

```json
{
  "name": "flags high-value orders",
  "workflow": "high-value-order.json",
  "input": { "order_id": "ORD-1", "total": 25000 },
  "stubs": { "http_call": { "crm": { "name": "Ada" } } },
  "expect": {
    "data.order.flagged": true,
    "data.order.customer_name": "Ada"
  }
}
```

| Field | Meaning |
|---|---|
| `name` | Reported in the output |
| `workflow` | Path to the workflow JSON, resolved **relative to the case file** |
| `input` | The bare payload |
| `stubs` | Inline connector stubs, same shape as the stub file |
| `stubs_file` | A stub file path instead, also relative to the case file |
| `metadata` | The request metadata, as the HTTP ingress would have built it |
| `expect` | **Rooted** dotted paths mapped to expected values |
| `expect_errors` | Expected task-error codes. **Defaults to empty** |
| `expect_calls` | Expected connector calls per function, in order |
| `expect_tasks` | The ids of the tasks that ran, in order. Unchecked when omitted |

`expect_errors` defaulting to empty is the load-bearing default: a workflow that
starts failing its tasks cannot pass silently. `expect_tasks` cannot work that
way — every workflow runs tasks — so omitting it means unchecked.

## Every `expect` path names its root

```json
"expect": {
  "data.order.flagged": true,
  "metadata.progress.status_code": 200,
  "temp_data.user_id": "u-1",
  "calls.mongo_write[0].input.collection": "sessions",
  "audit_trail[1].task_id": "persist"
}
```

| Root | Reads |
|---|---|
| `data.` | The data document |
| `metadata.` | The metadata document — request context, and whatever the workflow wrote there |
| `temp_data.` | The scratch document tasks pass values through |
| `calls.` | The connector calls the run made, grouped by function (below) |
| `audit_trail.` | One entry per executed task: `task_id`, `status`, `changes` |

Array positions work either way: `calls.mongo_write[0]` and
`calls.mongo_write.0` are the same path. An expected `null` matches an absent
path as well as an explicit one, because JSONLogic resolves a missing `var` to
null and that is already what the workflow sees.

> [!IMPORTANT]
> **The root is required** (changed in 1.2.0). A leading `data.` used to be
> optional, so `metadata.foo` silently read the data document's own `metadata`
> key, came back absent, and — since an expected `null` matches absent — could
> *pass*. Every other path in Orion already spells its root; this is the case
> file catching up. To migrate a suite:
>
> ```bash
> jq '.expect |= with_entries(
>       if (.key | test("^(data|metadata|temp_data|calls|audit_trail)([.\\[]|$)"))
>       then . else .key |= "data." + . end)' \
>   -S case.json
> ```
>
> A path that names no root fails the case before the workflow runs, and the
> error suggests the `data.` form.

## Assert on what a workflow writes

A stub answers a connector call, so nothing downstream sees what the task
*tried* to send. The run records it instead: every connector-backed call, with
its payload resolved the way the real handler resolves it.

```json
"expect_calls": {
  "mongo_write": [
    { "collection": "sessions",
      "update": { "$set": { "generation": 2, "revokedAt": null } } }
  ],
  "publish_kafka": []
}
```

- Entries match **positionally**, in execution order, as a **deep subset** —
  name the fields you care about and ignore the rest.
- The number of entries must equal the number of recorded calls for that
  function, so an unexpected extra write fails. `"publish_kafka": []` asserts
  nothing was published.
- Only the functions named are constrained.
- **Presence is strict here**, unlike `expect`: `"revokedAt": null` asserts the
  field was *written as null*, not that it is absent. A recorded payload is a
  literal document, so whether a field was written is the assertion.

`crypto`, `jwt_sign` and `jwt_verify` are not recorded — they execute for real
offline rather than through a stub, and their inputs can carry key material.

> [!TIP]
> This is what catches JSONLogic in a write payload. A connector field folds
> `{"var": …}` nodes and **nothing else**, so `{"if": […]}` in a `document` is
> stored as a literal BSON object. The recorded call shows the object, so
> `expect_calls` fails where a stubbed run used to pass. `orion-server lint`
> warns about the same thing statically.

## Supply request metadata

A workflow that branches on `metadata.headers`, reads `metadata.auth.claims`, or
uses `metadata.params` needs that context to be testable at all:

```json
{
  "name": "login: mobile device with a registered handset",
  "workflow": "auth-login.json",
  "metadata": {
    "headers": { "deviceid": "device-abc" },
    "auth":    { "claims": { "sub": "asha@example.com" } },
    "params":  { "id": "42" },
    "query":   { "page": "2" }
  },
  "input":  { "emailId": "asha@example.com" },
  "expect": { "data.mode": "device" }
}
```

The block is normalized the way the ingress builds one, so an offline pass means
a production pass:

- **Header keys are lowercased** — HTTP header names arrive lowercase, so
  `"DeviceId"` would match offline and miss in production.
- **Credential headers are masked** — `authorization`, `cookie`,
  `proxy-authorization` and `x-api-key` read back as `******`, exactly as they
  do in a served request.
- **`_orion_errors` is cleared**; it is engine-owned.

Any key is accepted, since the HTTP envelope merges caller-supplied metadata.
The reserved ones are shape-checked: `headers`/`params`/`query`/`cookies` must
be objects of strings, `channel`/`http_method` strings, and `auth` may carry
only `claims` — the request path builds `auth` as `{"claims": …}` and nothing
else reaches a workflow.

`dry-run --metadata <file>` takes the same object.

```bash
orion-server test ./workflow-tests
```

```
  ok    flags high-value orders
  FAIL  leaves small orders alone
          data.order.flagged: expected false, got true

1 passed, 1 failed (2 case(s))
```

Non-zero on any failure, so a suite gates CI the same way `lint` does. The
repository's own suite is `examples/workflow-tests/`, whose cases reference the
example packages' real workflow files rather than copies.

## Check the config and the backends

Two more gates worth running before a deploy, both of which need config rather
than workflows:

```bash
orion-server validate-config -c config.toml    # unknown keys, invalid values, the effective config
orion-server test-connectivity -c config.toml  # the database, and Kafka when enabled
orion-server preflight -c config.toml          # stored channels and workflows the current rules refuse
```

`validate-config` prints the full effective config — defaults plus file plus
`ORION_*` overrides — with secrets masked, so you can see what the process will
actually run with.

## Wire it into CI

```yaml
- name: Validate workflows
  run: |
    for f in workflows/*.json; do orion-server lint "$f" --deny-warnings; done
    orion-server test ./workflow-tests
```

No server, no database, no secrets — which is what makes this runnable on a
pull request from a fork. The deploy half of the pipeline is
[CI/CD with Packages](../guides/ci-cd.md).

## Related

- [CLI Reference](../reference/cli.md) — every flag of every subcommand.
- [Test & Promote a Service](../getting-started/test-and-promote.md) — the same
  commands as a walkthrough.
- [CI/CD with Packages](../guides/ci-cd.md) — the promotion pipeline these gates
  feed.
- [Author Workflows](./workflows.md) — what you are testing.
