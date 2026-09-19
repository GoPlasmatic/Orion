<!-- description: Run Orion workflows with no server, database or network: lint a definition set, dry-run with stubbed connectors, and keep case-file regression tests in CI. -->
<!-- type: guide -->
<!-- last_verified: 2026-09-14 -->

# Test a workflow offline

`orion-server` runs workflows without a server, a database or a network, so a workflow can be developed and regression-tested the way any other code is. This guide is the CI author's reference for those commands: format, advisory checks, lint, dry run, and a case-file suite.

## Before you start

You need `orion-server` on your `PATH` and a directory of definition files. Nothing here needs a server, a database or a secret, which is what makes it runnable on a pull request from a fork.

Four gates, cheapest first: `lint`, `dry-run`, `test` and `test-connectivity`. The first three need nothing but the binary and your JSON files. Before any of them, `fmt`. It is not a gate on correctness, but it is the reason a review diff shows what changed rather than how it was laid out.

## Format the files

Rewrite in place, or check in CI:

```bash
orion-server fmt ./definitions          # rewrite in place
orion-server fmt --check ./definitions  # CI: diff and exit 1 if anything would change
```

One style, no configuration; [Definition style](../../reference/fmt.md) is the whole of it. `fmt` never changes a value. The output is parsed again and compared with the input before a file is written.

## Ask what could be better

Run the advisory rules after lint's gate:

```bash
orion-server clippy ./definitions               # lint's gate, then the advisory rules
orion-server -c config.toml clippy ./definitions # + the rules that read [vars] and [secrets]
```

`clippy` says only what it is certain of. Its rules cover a workflow whose condition can never match, steps after an unconditional terminal step, and a call cycle that always fails. They also cover a parse whose result is always overwritten, and a run of steps an existing fragment already expresses. Every rule states its proof and when it stays silent, in [Advisory checks](../../reference/clippy/index.md). There is no configuration and no suppression, which is why the rules are few.

## Lint a workflow file

Point it at one file, or at the whole set:

```bash
orion-server lint workflow.json      # one file
orion-server lint ./definitions      # the whole set, and the references between files
```

Output:

```text
'workflow.json' is valid.
```

Pointing it at a directory validates every channel, workflow and connector under it *and* resolves the references between them. That catches a `channel_call` target that exists nowhere, a task naming a connector of the wrong type, and two channels claiming one route. Those cannot be caught one file at a time, because the file that would disprove them is one the command never opens. See [`lint`](../../reference/cli/orion-server/lint.md) for the flags.

`lint` applies the same validators the admin API applies on create: task shapes, function names, and each connector function's `input` schema. It exits non-zero on any error, which is all a pull-request gate needs.

It also prints advisory warnings on stderr without failing. Today there is one: JSONLogic in a connector field that folds `{"var": …}` and nothing else, so the expression is stored or sent verbatim. `--deny-warnings` turns those into a failure for a gate that wants them to block:

```bash
orion-server lint workflow.json --deny-warnings
```

It stays advisory by default because operator names such as `length`, `type`, `keys` and `in` are ordinary field names. A document that legitimately holds a stored rule is a real payload. `POST /api/v1/admin/workflows/validate` reports the same findings in its `warnings` array.

Every `orion-server` subcommand run without `-c` prints `Note: no config file specified…` on stderr. It is not a finding; redirect it or pass a config file.

## Dry-run against sample input

Run the workflow in an in-process engine:

```bash
orion-server dry-run -w workflow.json -i payload.json
```

`-i` takes the bare payload, not the `{"data": …}` envelope the HTTP API uses. `--metadata` takes a second file holding the request metadata the HTTP ingress would have built; see [Supply request metadata](#supply-request-metadata). The command prints one JSON document on stdout, so `jq` can read it:

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

The five documents after `trace` are the same set, in the same shape, that a case's `expect` roots address. A path read off a dry run works in a case unchanged. `output` is kept as an alias of `data` because CI `jq` filters read it. `calls` is grouped by function rather than flat; each record carries a `seq` if you need the order across functions.

Read the part you care about:

```bash
orion-server dry-run -w workflow.json -i payload.json | jq '.output.order'
```

It exits non-zero when the run fails, and prints the trace either way. A run that dies at task three still tells you what the first two did.

## Stub the calls that leave the process

Connector-backed tasks (`http_call`, `db_read`, `data_query`, `channel_call` and the rest) are answered from a *stub file* rather than a real backend:

```json
{
  "http_call":    { "crm": { "name": "Ada Lovelace" } },
  "data_query":   { "orders-db": [ { "id": 1, "total": 10 } ] },
  "channel_call": { "inventory-check": { "in_stock": true } },
  "db_write":     { "*": { "rows_affected": 1 } }
}
```

The outer key is the function name. The inner key is the task's `connector`, or its `channel` for `channel_call`, and `"*"` matches any target. The value is what the task writes to its `output` path:

```bash
orion-server dry-run -w workflow.json -i payload.json --stubs stubs.json
```

Stubbing is all-or-nothing. A task with no matching stub fails, and the error names the stub that would satisfy it. A half-stubbed run reporting success would be worse than no stubs at all, because it looks like a pass. Two mistakes are caught when the file is parsed rather than at the failing task. One is naming a function that is not connector-backed; the other is putting the response where the target map belongs.

> [!NOTE]
> This is the offline counterpart to `POST /workflows/{id}/test`, which runs the same workflow against *live* connectors: real webhooks, real databases, real topics. Reach for the endpoint when you mean to touch the real systems, and for `dry-run` when you do not.

## Run a model offline

A workflow that calls [`model_infer`](../../reference/functions/model_infer.md) can run the model for real, with no server and no bucket, when the model is on disk. Pass `--model-dir` pointing at a directory holding the manifest and the artifact its `artifact` field names beside it:

```bash
orion-server dry-run -w score.json -i board.json --model-dir ./models/c4-tiny
orion-server test ./workflow-tests --model-dir ./models
```

The model runs through the same handler a node registers: the manifest's adapters, the runtime, the result expression, every limit and every refusal. What the run writes is what the deployed workflow would write for the same bytes. What does *not* happen offline is admission. No digest is claimed and no probe runs, because the bytes are your own and the run is the probe.

A model the directory holds without its artifact, or a literal `model` id no manifest describes, is refused before anything runs. The error is `MODEL_ARTIFACT_UNAVAILABLE`, naming the model and what to add. A computed `model` resolves against the directory per message and fails the task as `unavailable` when it names one that is not there. Once a model directory is given, `model_infer` is never stubbed.

Without the flag, `model_infer` is stubbed like a connector function, keyed by its name. `{"model_infer": {"*": {"policy": [[0.1, 0.2, 0.7, 0, 0, 0, 0]]}}}` is what the task writes to its `output`, or to `temp_data.inference` when it names none. A workflow that calls it with no stub either is refused with the same code rather than left to fail at the task.

`orion-server lint` reads the same directory. A `model.json` in the set, or a `--model-dir`, is what a literal `model_infer` reference is checked against. With the artifact beside it, the graph's parameter count and tensor names are reported before anything is submitted.

## Build a regression suite

A case is any `*.case.json` file; the suffix is what separates cases from the workflows and fixtures beside them:

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
| `workflow` | Path to the workflow JSON, resolved relative to the case file |
| `input` | The bare payload |
| `stubs` | Inline connector stubs, same shape as the stub file |
| `stubs_file` | A stub file path instead, also relative to the case file |
| `secrets` | Stand-in values for the `{"secret": "name"}` references the workflow reads, same shape as `dry-run --secrets` |
| `metadata` | The request metadata, as the HTTP ingress would have built it, including `vars` |
| `expect` | Rooted dotted paths mapped to expected values |
| `expect_errors` | Expected task-error codes. Defaults to empty |
| `expect_calls` | Expected connector calls per function, in order |
| `expect_tasks` | The ids of the tasks that ran, in order. Unchecked when omitted |

`expect_errors` defaulting to empty is the load-bearing default: a workflow that starts failing its tasks cannot pass silently. `expect_tasks` cannot work that way, because every workflow runs tasks, so omitting it means unchecked.

## Test a refusal

A case that names codes in `expect_errors` is asserting the failure, so the run ending on that error is the pass. That is how a refusal gets a test. The codes are still matched exactly, so a case cannot pass by expecting the wrong ones. A task that ran and failed is listed in `expect_tasks` like any other; the tasks after it are not, because the workflow halted there:

```json
{
  "name": "a record shorter than the spec is refused, not half-parsed",
  "workflow": "../packages/fixed-width-statement/workflow.json",
  "input": { "record": "ACC0001234" },
  "expect": { "data.statement": null },
  "expect_errors": ["VALIDATION_ERROR", "WORKFLOW_ERROR"],
  "expect_tasks": ["parse", "decode"]
}
```

## Root every `expect` path

Since Orion 1.2, every path names its root:

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
| `metadata.` | The metadata document: request context, and whatever the workflow wrote there |
| `temp_data.` | The scratch document tasks pass values through |
| `calls.` | The connector calls the run made, grouped by function |
| `audit_trail.` | One entry per executed task: `task_id`, `status`, `changes` |

Array positions work either way: `calls.mongo_write[0]` and `calls.mongo_write.0` are the same path. An expected `null` matches an absent path as well as an explicit one. JSONLogic resolves a missing `var` to null, and that is already what the workflow sees.

A leading `data.` used to be optional, so `metadata.foo` silently read the data document's own `metadata` key, came back absent, and could *pass*. A path that names no root now fails the case before the workflow runs, and the error suggests the `data.` form. To migrate a suite:

```bash
jq '.expect |= with_entries(
      if (.key | test("^(data|metadata|temp_data|calls|audit_trail)([.\\[]|$)"))
      then . else .key |= "data." + . end)' \
  -S case.json
```

## Assert on what a workflow writes

A stub answers a connector call, so nothing downstream sees what the task *tried* to send. The run records it instead: every connector-backed call, with its payload resolved the way the real handler resolves it:

```json
"expect_calls": {
  "mongo_write": [
    { "collection": "sessions",
      "update": { "$set": { "generation": 2, "revokedAt": null } } }
  ],
  "publish_kafka": []
}
```

- Entries match positionally, in execution order, as a deep subset. Name the fields you care about and ignore the rest.
- The number of entries must equal the number of recorded calls for that function, so an unexpected extra write fails. `"publish_kafka": []` asserts nothing was published.
- Only the functions named are constrained.
- Presence is strict here, unlike `expect`. `"revokedAt": null` asserts the field was *written as null*, not that it is absent. A recorded payload is a literal document, so whether a field was written is the assertion.

`crypto`, `jwt_sign` and `jwt_verify` are not recorded. They execute for real offline rather than through a stub, and their inputs can carry key material.

`expect_calls` matches against the call's `input`. The `expect` block reaches the whole record, which carries a little more:

| Path | Holds |
|---|---|
| `calls.<fn>[i].input` | The resolved payload, which `expect_calls` compares against |
| `calls.<fn>[i].task_id` | The task that made the call |
| `calls.<fn>[i].seq` | Position across all functions, so two functions' calls can be ordered against each other |
| `calls.<fn>[i].stub_target` | The key that matched in the stub table: the task's `connector`, or its `channel` for `channel_call` |

`task_id` is what to assert when two tasks call the same function and only one of them should have fired:

```json
"expect": { "calls.http_call[0].task_id": "notify_customer" }
```

> [!TIP]
> This is what catches JSONLogic in a write payload. A connector field folds `{"var": …}` nodes and nothing else, so `{"if": […]}` in a `document` is stored as a literal BSON object. The recorded call shows the object, so `expect_calls` fails where a stubbed run used to pass. `orion-server lint` warns about the same thing statically.

## Supply request metadata

A workflow that branches on `metadata.headers`, reads `metadata.auth.claims`, or uses `metadata.params` needs that context to be testable at all:

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

The block is normalized the way the ingress builds one, so an offline pass means a production pass:

- Header keys are lowercased. HTTP header names arrive lowercase, so `"DeviceId"` would match offline and miss in production.
- Credential headers are masked. `authorization`, `cookie`, `proxy-authorization` and `x-api-key` read back as `******`, exactly as they do in a served request.
- `_orion_errors` is cleared; it is engine-owned.

Any key is accepted, since the HTTP envelope merges caller-supplied metadata. The reserved ones are shape-checked: `headers`, `params`, `query` and `cookies` must be objects of strings, `vars` an object, `channel` and `http_method` strings. `auth` may carry only `claims`, because the request path builds `auth` as `{"claims": …}` and nothing else reaches a workflow.

`vars` is passed through rather than stamped. Offline there is no config file to read `[vars]` from, so a case supplies them the way it supplies headers. The shape is still checked, because the serving path force-stamps the key from one object. `dry-run --metadata <file>` takes the same object.

## Verify

Run the suite and read the summary line:

```bash
orion-server test ./workflow-tests
```

Output, for a suite with one failing case:

```text
  ok    flags high-value orders
  FAIL  leaves small orders alone
          data.order.flagged: expected false, got true

1 passed, 1 failed (2 case(s))
```

It exits non-zero on any failure, so a suite gates CI the same way `lint` does. The repository's own suite is `examples/workflow-tests/`, whose cases reference the example packages' real workflow files rather than copies.

## Check the config and the backends

Two more gates are worth running before a deploy, and both need config rather than workflows:

```bash
orion-server validate-config -c config.toml    # unknown keys, invalid values, the effective config
orion-server test-connectivity -c config.toml  # the database, and Kafka when enabled
orion-server preflight -c config.toml          # stored channels and workflows the current rules refuse
```

`validate-config` prints the full effective config, which is the defaults plus the file plus `ORION_*` overrides, with secrets masked. You can see what the process runs with.

## Check the SQL against a real schema

`lint` never sees the schema a `db_read` or `db_write` statement runs on. [`orion-server sql check`](../../reference/cli/orion-server/sql-check.md) prepares every statement as its connector's role and executes nothing. In CI, build the schema from the migrations in a transaction that is rolled back:

```bash
orion-server sql check ./definitions --schema ./migrations --database "$ADMIN_DATABASE_URL"
```

On PostgreSQL 16 or later it also proves each connector's role holds the grants its statements need.

## Wire it into CI

One job, no server, no database, no secrets:

```yaml
- name: Validate workflows
  run: |
    orion-server fmt --check workflows
    for f in workflows/*.json; do orion-server lint "$f" --deny-warnings; done
    orion-server clippy workflows
    orion-server test ./workflow-tests
```

The deploy half of the pipeline is [CI/CD with packages](../patterns/ci-cd.md).

## Next steps

- [CLI](../../reference/cli/index.md): every flag of every subcommand.
- [Test and promote a service](../../get-started/tutorials/test-and-promote.md): the same commands as a walkthrough.
- [CI/CD with packages](../patterns/ci-cd.md): the promotion pipeline these gates feed.
- [Author a workflow](./workflows.md): what you are testing.
