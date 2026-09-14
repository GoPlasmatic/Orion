<!-- description: The model_infer task function: run an admitted ONNX model through its manifest's adapters, with the runtime, deadline, failure categories and offline mode. -->
<!-- type: reference -->
<!-- last_verified: 2026-09-14 -->

# `model_infer`

Runs an admitted [model](../admin-api/models.md), an ONNX artifact this node has fetched, verified and probed, and writes what the model's manifest makes of the outputs. The **manifest** does the marshalling. One *adapter* per declared input turns `input` into that input's tensor, and the *result* expression turns the output tensors back into JSON.

## Synopsis

```json
{
  "name": "model_infer",
  "input": {
    "model": "ada.c4-tiny",
    "input": {
      "var": ""
    },
    "runtime": "…",
    "output": "data.policy",
    "raw": false,
    "timeout_ms": 0,
    "stats_output": "temp_data.inference"
  }
}
```

## Description

`model_infer` is a compute function. It runs a model this node has admitted and names no connector.

**Retry safety:** `pure`. See [Retry safety](./retry-safety.md) for what the answer costs.

The task names the model, hands over the JSON root the adapters read, and says where the result goes. The dtypes, the shapes and the expressions are the manifest's, so a workflow never spells a tensor. The adapters and the result are compiled on the serving generation's own expression engine. That is what lets them use the [tensor family](../expressions.md#tensors-tensor) and be priced by [`engine.ops_budget`](../configuration/engine.md).

## Fields

| Field | Type | Required | Default | Description |
|-------|------|:--------:|---------|-------------|
| `model` | string \| JSONLogic | yes | — | The model id; the active version resolves. JSONLogic here is what lets one workflow route to any model |
| `input` | any \| JSONLogic | yes | — | The JSON root every input adapter of the manifest sees. `{"var": ""}` hands the adapters the whole message context (`data`, `metadata`, `temp_data`) |
| `runtime` | string | no | `[models.default_runtime]` for the model's format | Which runtime runs the graph: one of the compiled-in names (`tract`). A name this build does not know is refused when the workflow is written (`MODEL_RUNTIME_UNKNOWN`); a known one disabled on a node fails the call there |
| `output` | string | no | `"temp_data.inference"` | Dotted result path |
| `raw` | bool | no | `false` | Skip `result`; write `{name: tensor}` in wire form for chaining — `{"policy": {"tensor": {"dtype": "f32", "shape": [1, 7], "data": "<base64>"}}}` |
| `timeout_ms` | number \| JSONLogic | no | the model's ceiling | Per-call deadline, capped by `models.max_timeout_ms` (or the model's `[[models.overrides]]` row); a cold load on first use is charged to it. JSONLogic here is what lets a workflow divide one wall-clock budget between several inferences. A value that is not a positive integer fails the call; the cap is the host's either way |
| `stats_output` | string | no | not written | Path for `{id, version, digest, runtime, device, parameters, artifact_bytes, ops, peak_ops, queued_ms, inference_ms, cold_load}`. `ops` is what the manifest's expressions charged for this call and `peak_ops` the heaviest single one, which is the number [`engine.ops_budget`](../configuration/engine.md#ops_budget) is compared against |

## Errors

**What the node checks, and how it refuses.** Nothing is written on a
failure, and every failure is one of these categories — the label
`orion_model_failures_total` counts by:

| Category | When | Response |
|---|---|---|
| `caller_input` | `model` does not name a model, `input` is missing, or an adapter produced something other than the declared tensor — the wrong dtype or shape, or no tensor at all | `400`, the message names the input and the expected `dtype[shape]` |
| `unavailable` | models are disabled on this node, the id has no active admitted version here, or the artifact could not be loaded into the runtime (the stage is named; the runtime's own text goes to the log) | `500` |
| `runtime_unavailable` | the runtime named — or the default for the format — is not enabled on this node, or does not serve the format | `500` |
| `adapter` | an adapter or the result expression failed to evaluate; an `engine.ops_budget` refusal keeps its `BUDGET_EXCEEDED` code | `400` |
| `input_size` / `output_size` | more elements than `models.max_input_elements` / `max_output_elements` | `400` |
| `permit` | no inference slot freed up before the deadline (`models.max_concurrent_inferences`, `models.max_concurrency_per_model`) | `400` |
| `timeout` | the deadline elapsed — loading, waiting or running | `504`-class, the one retryable category |
| `run` | the runtime failed mid-graph, or produced outputs that do not match the manifest | `500` |

## Examples

```json
{
  "name": "model_infer",
  "input": {
    "model": "ada.c4-tiny",
    "input": { "var": "" },
    "output": "data.policy",
    "stats_output": "temp_data.inference"
  }
}
```

Take the `c4-tiny` manifest: one input `board` (`f32[1,2,6,7]`, adapter `{"tensor": [{"var": "data.board"}, "f32"]}`), one output `policy` (`f32[1,7]`), and the result `{"policy": {"to_list": [{"var": "policy"}]}}`. A message whose `data.board` is the nested list of a board gets `data.policy.policy` back as one row of seven numbers. A computed `model` routes per message. The dependants list on `GET /models/{id}/dependencies` and the quarantine below see only literal ids.

## Caveats

The adapters run **before** the load, so a message that does not marshal never pays a cold load. When the session is cold, the load counts against the call's deadline. Under `models.preload = "referenced"` (the default) a node warms every model an active workflow names right after publishing a generation. The first request therefore rarely finds one cold.

**Availability is decided at load, not at the first request.** A workflow that names a model by literal id is [quarantined](../admin-api/models.md) while the node's generation cannot serve that model. Its channels are refused with a `503` naming the model and why. The reasons are no active admitted version, `models.enabled = false`, or an adapter the serving engine refuses. `/health`
lists the reason under `models.failed_to_load` and `channels.quarantined`.
A computed `model` is checked per message instead and answers
`unavailable`.

**Offline.** With [`--model-dir`](../cli/orion-server/dry-run.md) pointing at the
manifest and its artifact, `dry-run` and `orion-server test` run the model **for real** through this same handler. The adapters, the runtime, the result expression and every refusal above all apply. There is no admission: the digest is computed from the file rather than claimed, and the bytes are trusted as the author's own. A workflow naming a model the directory does not hold is refused before it runs with `MODEL_ARTIFACT_UNAVAILABLE`. Without the flag `model_infer` is stubbed like a connector function, keyed by its name. The stub file's `"model_infer": {"*": …}` entry is what the task writes at `output`. See [Run a model offline](../../guides/author/testing.md#run-a-model-offline).

## Related

- [Models](../../concepts/models.md): what a model is, how it is admitted, and what the manifest owns.
- [Serve a model](../../guides/extend/models.md): registering, admitting and calling a model end to end.
- [Task functions](./index.md): the whole catalogue, and each function's retry safety.
- [Expression language › Tensors](../expressions.md#tensors-tensor): the operators a manifest's adapters are written in.
