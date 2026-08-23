# Workflow tests

Offline regression tests for the workflows in this directory's siblings, run by:

```bash
orion-server test examples/workflow-tests
```

Every case runs the real workflow JSON through the real engine with no server,
no database and no network. Connector-backed tasks are answered from a `stubs`
block — `channel-composition-vip.case.json` stubs the `channel_call` its
workflow makes, which is how a composed service is tested without deploying the
service it calls.

## Writing a case

A case is a `*.case.json` file. The suffix is what tells the runner a file is a
case rather than a workflow or a fixture, so all three can live side by side.

```json
{
  "name": "flags an order above the threshold",
  "workflow": "../packages/high-value-order/workflow.json",
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
| `metadata` | Request metadata, as the HTTP ingress builds it: `headers`, `params`, `query`, `cookies`, `auth.claims`, `channel`. Header keys are lowercased and credential headers masked. |
| `expect` | Rooted dotted path → expected value. The root — `data.`, `metadata.`, `temp_data.`, `calls.` or `audit_trail.` — is **required**. An expected `null` also matches an absent path. |
| `expect_errors` | Expected task-error codes, in order. Defaults to empty, and is checked either way — so a workflow that starts failing cannot pass quietly. |
| `expect_calls` | Expected connector calls per function, in order, each a deep subset of the call's resolved payload. The count must match. Presence is strict here: `null` means *written as null*. |
| `expect_tasks` | The ids of the tasks that ran, in order, matched exactly. Unchecked when omitted. |

The runner exits non-zero on any failure, so it gates CI alongside
`orion-server lint`.
