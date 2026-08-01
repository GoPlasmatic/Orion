# Workflow tests

Offline regression tests for the workflows in this directory's siblings, run by:

```bash
orion-server test examples/workflow-tests
```

Every case runs the real workflow JSON through the real engine with no server,
no database and no network. Connector-backed tasks would be answered from a
`stubs` block; these examples are self-contained and need none.

## Writing a case

A case is a `*.case.json` file. The suffix is what tells the runner a file is a
case rather than a workflow or a fixture, so all three can live side by side.

```json
{
  "name": "flags an order above the threshold",
  "workflow": "../high-value-order/workflow.json",
  "input": { "order_id": "ORD-9182", "total": 25000 },
  "stubs": { "http_call": { "crm": { "name": "Ada" } } },
  "expect": {
    "data.order.flagged": true,
    "data.order.alert": "High-value order: $25000"
  }
}
```

| Field | Meaning |
|---|---|
| `name` | Reported name. Defaults to the file name without `.case.json`. |
| `workflow` | Path to the workflow, relative to the case file. |
| `input` | The message payload. |
| `stubs` | Canned connector responses, inline. `stubs_file` points at one instead. |
| `expect` | Dotted output path → expected value. A leading `data.` is optional, and an expected `null` also matches an absent path. |
| `expect_errors` | Expected task-error codes, in order. Defaults to empty, and is checked either way — so a workflow that starts failing cannot pass quietly. |

The runner exits non-zero on any failure, so it gates CI alongside
`orion-server lint`.
