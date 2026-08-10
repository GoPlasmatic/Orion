# Test Workflows Offline

`orion-server` runs workflows without a server, a database, or a network, so a
workflow can be developed and regression-tested the way any other code is. This
page is the CI author's reference for those commands.

Four gates, cheapest first: `lint`, `dry-run`, `test`, and `test-connectivity`.
The first three need nothing but the binary and your JSON files.

## Lint a workflow file

```bash
orion-server lint workflow.json
```

```
'workflow.json' is valid.
```

`lint` applies the same validators the admin API applies on create: task shapes,
function names, and each connector function's `input` schema. It exits non-zero
on any finding, which is all a pull-request gate needs.

Every `orion-server` subcommand run without `-c` prints
`Note: no config file specified…` on **stderr**. It is not a finding; redirect
it or pass a config file.

## Dry-run against sample input

```bash
orion-server dry-run -w workflow.json -i payload.json
```

`-i` takes the **bare payload** — not the `{"data": …}` envelope the HTTP API
uses. The command prints one JSON document on stdout, so `jq` can read it:

| Field | Holds |
|---|---|
| `matched` | Whether any task ran |
| `trace` | The per-task execution path, including which tasks were skipped |
| `output` | The final data context |
| `errors` | Task errors, if any |

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
| `expect` | Dotted output paths mapped to expected values; a leading `data.` is optional |
| `expect_errors` | Expected task-error codes. **Defaults to empty** |

`expect_errors` defaulting to empty is the load-bearing default: a workflow that
starts failing its tasks cannot pass silently.

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
    for f in workflows/*.json; do orion-server lint "$f"; done
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
