<!-- description: orion-server dry-run executes a workflow offline against a JSON input with canned connector stubs, real plugin and model execution, and prints the trace. -->
<!-- type: reference -->
<!-- last_verified: 2026-09-14 -->

# `orion-server dry-run`

Executes a workflow against a JSON input in an in-process engine, then prints the per-task execution trace. Connector-backed tasks are answered from `--stubs`. Without a matching stub the task fails and names the stub it needs.

## Synopsis

```bash
orion-server dry-run -w <workflow.json> -i <input.json> [--stubs <stubs.json>] [--metadata <metadata.json>] [--secrets <secrets.json>]
```

## Description

The printed document carries `data`, `metadata`, `temp_data`, `audit_trail` and `calls`. Those are the same five documents, in the same shape, that a case's `expect` roots address. It also carries `output` (an alias of `data`, kept for existing `jq` filters), `trace`, `matched` and `errors`.

## Options

| Flag | Description |
|------|-------------|
| `-w, --workflow` | Path to a workflow JSON file. |
| `-i, --input` | Path to a JSON file used as the message payload. |
| `-s, --stubs` | Path to a JSON file of canned connector responses. The inner key is the task's `connector` (or `channel` for `channel_call`); `"*"` matches any. |
| `--definitions` | Directory holding the set's shared definitions, resolved before validation. |
| `--plugin-dir` | Directory of plugin manifests and their components. A plugin function runs **for real** in the sandbox — it is capability-free, like `crypto` — never stubbed. A workflow naming a plugin function whose manifest is given but whose component is not beside it fails as `PLUGIN_ARTIFACT_UNAVAILABLE`; one naming a function no manifest covers is refused by name. Repeatable. |
| `--model-dir` | Directory of model manifests and their artifacts. With one, `model_infer` runs the model **for real** — the same handler a node registers, over the file beside the manifest, on the engine's own runtime — never stubbed: a workflow naming a model the directory does not hold, or holds without its artifact, is refused before it starts as `MODEL_ARTIFACT_UNAVAILABLE`, and a computed `model` resolves against the directory per message. Without one, `model_infer` is answered from `--stubs` by function name (`{"model_infer": {"*": <result>}}`), and a workflow that calls it with no stub either is refused with the same code. No admission runs offline: the digest is computed from the file, never claimed, and the bytes are trusted as the author's own. Repeatable. |
| `-m, --metadata` | Path to a JSON file used as the message metadata — `headers`, `params`, `query`, `cookies`, `auth.claims`, `channel`, `vars`. Header keys are lowercased and credential headers masked, as at the HTTP ingress. |
| `--secrets` | Path to a JSON object of stand-in values for the `{"secret": "name"}` references the workflow reads: `{"partner_hmac": "test-key"}`. Offline there is no `[secrets]` config to resolve, and an engine with no store refuses a workflow that names one. Values are used verbatim — use throwaway ones. |

## Examples

```bash
orion-server dry-run -w wf.json -i input.json --stubs stubs.json
```

## Related

- [Test a workflow offline](../../../guides/author/testing.md): stubs, metadata and secrets files in practice.
- [`orion-server test`](./test.md): the same execution, as a regression suite.
- [Workflow definition](../../workflows.md): the document this command executes.
- [`orion-server` commands](./index.md): every `orion-server` subcommand.
