<!-- description: orion-cli dry-run executes a workflow offline against a JSON input with canned connector stubs, real plugin execution, and prints the trace. -->
<!-- type: reference -->
<!-- last_verified: 2026-09-25 -->

# `orion-cli dry-run`

Executes a workflow against a JSON input in an in-process engine, then prints the per-task execution trace. Connector-backed tasks are answered from `--stubs`. Without a matching stub the task fails and names the stub it needs.

## Synopsis

```bash
orion-cli dry-run -w <workflow.json> -i <input.json> [--stubs <stubs.json>] [--metadata <metadata.json>] [--secrets <secrets.json>] [--trace full|steps|none]
```

## Description

With `--definitions`, a set whose [`package` document](../shared-definitions.md#the-package-document) declares a `requires.orion` range excluding this binary stops before anything runs. One line names the range and this version.

A computed `output` that resolves to `null` currently fails in the offline stub
layer, even for task functions whose serving implementation falls back to `data`.
The call does not reach the call log. Use an explicit destination such as
`"output": "data"` when that fallback is intended. Matching the serving policy
requires the upstream resolver extension tracked in
[dataflow-rs #83](https://github.com/GoPlasmatic/dataflow-rs/issues/83).

The printed document carries `data`, `metadata`, `temp_data`, `audit_trail` and `calls`. Those are the same five documents, in the same shape, that a case's `expect` roots address. It also carries `output`, an alias of `data` kept for existing `jq` filters. `tasks` lists the ids of the tasks that ran, once per loop sweep and per `for_each` element. Last come `matched` (whether any task ran), `errors`, and `trace` unless `--trace none`.

Each step in the trace snapshots the message after it, carrying only the audit entry that step wrote. The trace grows with the steps rather than with their square. For a long looping workflow, `--trace steps` drops the snapshots and `--trace none` runs the workflow as a node runs an untraced message.

## Options

| Flag | Description |
|------|-------------|
| `-w, --workflow` | Path to a workflow JSON file. |
| `-i, --input` | Path to a JSON file used as the message payload. |
| `-s, --stubs` | Path to a JSON file of canned connector responses. The inner key is the task's `connector` (or `channel` for `channel_call`); `"*"` matches any. |
| `--definitions` | Directory holding the set's shared definitions, resolved before validation. |
| `--plugin-dir` | Directory of plugin manifests and their components. A plugin function runs **for real** in the sandbox — it is capability-free, like `crypto` — never stubbed. A workflow naming a plugin function whose manifest is given but whose component is not beside it fails as `PLUGIN_ARTIFACT_UNAVAILABLE`; one naming a function no manifest covers is refused by name. Repeatable. |
| `-m, --metadata` | Path to a JSON file used as the message metadata — `headers`, `params`, `query`, `cookies`, `auth.claims`, `channel`, `vars`. Header keys are lowercased and credential headers masked, as at the HTTP ingress. |
| `--trace` | How much of the run to record. `full` (the default): every step, with a snapshot of the message after it and the changes it made. `steps`: every step's id, result, timing and changes, with no snapshots. `none`: no trace and no per-write capture, as a node runs a message whose channel does not trace task details; `audit_trail` entries then carry no `changes`. |
| `--secrets` | Path to a JSON object of stand-in values for the `{"secret": "name"}` references the workflow reads: `{"partner_hmac": "test-key"}`. Offline there is no `[secrets]` config to resolve, and an engine with no store refuses a workflow that names one. Values are used verbatim — use throwaway ones. |

## Examples

```bash
orion-cli dry-run -w wf.json -i input.json --stubs stubs.json
```

## Related

- [Test a workflow offline](../../../guides/author/testing.md): stubs, metadata and secrets files in practice.
- [`orion-cli test`](./test.md): the same execution, as a regression suite.
- [Workflow definition](../../workflows.md): the document this command executes.
- [`orion-cli` commands](./index.md): every `orion-cli` subcommand.
