# Workflow tests

Offline regression tests for the workflows in this directory's siblings, run by:

```bash
orion-server test examples/workflow-tests \
  --plugin-dir examples/packages/fixed-width-statement --plugin-dir examples/packages/c4-tournament \
  --model-dir examples/packages/c4-tournament/entrant
```

Every case runs the real workflow JSON through the real engine with no server,
no database and no network. Connector-backed tasks are answered from a `stubs`
block — `channel-composition-vip.case.json` stubs the `channel_call` its
workflow makes, which is how a composed service is tested without deploying the
service it calls. A plugin function runs for real from the `--plugin-dir` that
holds its manifest and component, never from a stub. `model_infer` runs for
real from a `--model-dir` holding the manifest and artifact of the model the
task names — the three `c4-*` cases run the tournament's reference entrant,
which is deterministic on the CPU, so a case can assert the cell its answer
lands in. Without the flag the function is answered from a stub by its own
name (`"model_infer": { "*": { "column": [3] } }`, as
`examples/packages/c4-tournament/stubs.json` does for a dry run), and a case
that names a model with neither fails rather than passing on nothing.

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
| `expect_errors` | Expected task-error codes, in order. Defaults to empty, and is checked either way — so a workflow that starts failing cannot pass quietly. Naming codes asserts the failure: the run ending on them is the pass, which is how `fixed-width-statement-short-record.case.json` tests a refusal. |
| `expect_calls` | Expected connector calls per function, in order, each a deep subset of the call's resolved payload. The count must match. Presence is strict here: `null` means *written as null*. |
| `expect_tasks` | The ids of the tasks that ran, in order, matched exactly. A task that ran and failed is one of them; the tasks after it are not, because the workflow halted there. Unchecked when omitted. |

The runner exits non-zero on any failure, so it gates CI alongside
`orion-server lint`.
