<!-- description: Serve an ONNX model: write the manifest, check it offline, put the bytes in a bucket, register and admit it, and call it from a workflow with model_infer. -->
<!-- type: guide -->
<!-- last_verified: 2026-09-14 -->

# Serve a model

A model adds an inference to a workflow without a model server: an ONNX graph Orion fetches from your bucket, verifies, probes and runs. One task calls it, and that task's input and output are ordinary JSON. This guide is the build, from the manifest to the promotion.

The manifest does the marshalling. Its adapters build the tensors and its result expression reads them back. The workflow never spells a tensor, and the node measures what it serves. [Models](../../concepts/models.md) is the concept page.

## Before you start

You need a trained ONNX graph, an S3-compatible bucket and a client for it, and `orion-server` and `orion-cli`. The server must run with `models.enabled = true` and a `cache_dir`. A node that configures `[models.trust]` also needs a signing key; see [Configuration › Models](../../reference/configuration/models.md).

## 1. Decide it is a model

A model is for a small graph whose answer is needed on every message: a score over a feature vector, a classifier, a policy. Two tests, in order:

- **Does the graph fit beside the workflow?** The node loads it whole, under `models.max_loaded_bytes`, and runs it on the CPU by default. A graph that needs a GPU-class budget, batching or a token stream is a service you reach with `http_call`.
- **Does it have weights?** A transformation without them is a [plugin](./plugins.md) or a JSONLogic expression, which need no runtime.

## 2. Write the manifest

The manifest is a JSON document beside the artifact. It names the model, declares every input and output the graph has, and says how JSON becomes those tensors and how the outputs become JSON:

```json
{
  "abi": "orion:model@1.0.0",
  "name": "example.c4-tiny",
  "version": "0.1.0",
  "format": "onnx",
  "artifact": "c4-tiny.onnx",
  "inputs": [
    {
      "name": "board",
      "dtype": "f32",
      "shape": [1, 2, 6, 7],
      "adapter": {
        "reshape": [
          { "transpose": [{ "crop": [{ "one_hot": [{ "var": "cells" }, 3, "f32"] }, [0, 1], [42, 2]] }, [1, 0]] },
          [1, 2, 6, 7]
        ]
      }
    }
  ],
  "outputs": [{ "name": "policy", "dtype": "f32", "shape": [1, 7] }],
  "result": { "column": { "argmax": [{ "var": "policy" }, 1] } }
}
```

The *adapter* is evaluated against the JSON the task hands over and must produce exactly the declared tensor. This one reads 42 cell values and one-hot encodes them into `[42, 3]`. It drops the "empty" plane, then turns the result into the `[1, 2, 6, 7]` plane stack the graph was trained on. That is four [tensor operators](../../reference/expressions.md#tensors-tensor), each priced by the data it moves. The *result* is evaluated against the outputs by name and produces what the task writes. Here that is the index of the largest policy entry, which for a `[1, 7]` tensor along axis 1 is a one-element list. Leave either out and the default applies: `{"tensor": [{"var": name}, dtype]}` for an input, a nested list for an output.

`name` is the model id: lowercase labels joined by `.`, with `orion.*` reserved. `version` is yours and informational; Orion assigns the entity version. `artifact` is the file beside the manifest, read by offline tooling and the CLI only. A registration names the bytes by bucket, key and digest. An adapter may not read `{"secret": …}`, `now` or `random`: a replay must reproduce the same tensors, and a manifest's author is not the secrets' owner.

### A variable axis

A dimension may be a **name** instead of a count, for a graph exported with a dynamic axis:

```json
{
  "inputs": [{ "name": "x", "dtype": "f32", "shape": ["N", 3] }],
  "outputs": [{ "name": "y", "dtype": "f32", "shape": ["N", 3] }],
  "probe_dims": { "N": 2 }
}
```

A name binds to whatever the call brings on its first occurrence. Every later occurrence — in another input, or in an output — must equal that binding. So `["N", 3]` on the output means *the same* N the input had, while the 3 stays exact: naming one axis costs nothing on the others. One session serves every size, so this is not the same as registering the model once per shape. What bounds a single call is `models.max_input_elements` rather than the declaration.

`probe_dims` is only for admission, which needs concrete tensors to run its five probe inferences. A name it leaves out is probed at 1. A graph needing more — a convolution with a kernel wider than its input — says so there. The binding is recorded in `stats.probe_dims`, because `probe_ms` over a variable axis means nothing without the size behind it.

## 3. Validate it offline

Point the offline runners at the directory holding the manifest and the artifact:

```bash
orion-server lint ./definitions --model-dir ./entrant
orion-server test ./cases --plugin-dir ./plugin --model-dir ./entrant
```

`lint` checks a `model_infer` task's literal `model` against a manifest it can see, and the manifest against the graph beside it. `test` and `dry-run` run the inference for real rather than stubbing it. A case naming a model the directory does not hold fails as `MODEL_ARTIFACT_UNAVAILABLE`, never on a silently passing stub. Without the flag the function is stubbed like a connector; a `"model_infer": {"*": …}` entry in the stubs file is what the task writes. That is how a workflow around a model is tested before the model exists.

No admission runs offline. The digest is computed from the file, and the bytes are trusted as your own. [Test a workflow offline](../author/testing.md#run-a-model-offline) covers the case format.

## 4. Put the bytes in the bucket

Any S3-compatible client will do. Orion's part is a [`storage` connector](../../reference/connectors/storage.md) that can read the object, with `operations.presign_get` on, which is the default:

```bash
aws s3 cp entrant/c4-tiny.onnx s3://models/c4-tiny.onnx
orion-cli connectors create -d '{"name":"models","connector_type":"storage","config":{
  "type":"storage","endpoint":"https://s3.eu-west-1.amazonaws.com","region":"eu-west-1",
  "bucket":"models","access_key":"env://MODELS_ACCESS_KEY","secret_key":"env://MODELS_SECRET_KEY"}}'
```

The node fetches through this connector at admission, and again on any node that has never seen the digest. The connector has to exist on every instance the model is promoted to.

## 5. Register it, and wait

Register the manifest with the reference, and follow the verdict:

```bash
orion-cli models create -f entrant/model.json --connector models --key c4-tiny.onnx \
  --digest "sha256:$(shasum -a 256 entrant/c4-tiny.onnx | cut -d' ' -f1)" --wait
```

The registration carries the manifest and the reference, never the bytes, and answers at once with `admission.state = "pending"`. The node then fetches the object, confirms the digest, reads the graph and runs the probe. `--wait` follows the verdict, printing each state change. It exits `0` on `passed`, `1` on `failed` with the stage and the reason, and `2` if `--timeout` elapses first. The same command therefore gates a pipeline. `orion-cli models get <id>` shows the verdict and the stats the node measured: `parameters`, the opset, the artifact size. `orion-cli models admit <id> --wait` runs admission again after a fix.

## 6. Activate it

Activate once the verdict is `passed`:

```bash
orion-cli models activate example.c4-tiny
```

It is refused with a `409` until admission has passed. Activation rebuilds the engine. Under the default `models.preload = "referenced"` the node warms the session for every active workflow that names the model, so the first request rarely finds it cold.

## Verify

Ask the node what it holds:

```bash
orion-cli models get example.c4-tiny
```

The row shows `status: active`, `admission.state: passed`, and under `health` this node's view: `admitted`, or `loaded` with the runtime and device once something has asked for it.

## 7. Call it from a workflow

Name the model, the JSON root the adapters read, and where the result goes:

```json
{
  "id": "infer",
  "name": "Ask the model for a column",
  "function": {
    "name": "model_infer",
    "input": {
      "model": "example.c4-tiny",
      "input": { "var": "temp_data.view" },
      "timeout_ms": 50,
      "output": "data.answer",
      "stats_output": "data.inference"
    }
  }
}
```

`input` is the JSON root every adapter reads: `{"var": ""}` hands over the whole context, a path hands over one object. A literal `model` is what the dependency list, the quarantine and the preload see. A computed one (`{"var": "data.mover_model"}`) routes per message and is checked per message instead. `timeout_ms` is the call's deadline, capped by `models.max_timeout_ms`, and a cold load is charged to it. It is JSONLogic too, so a workflow running several inferences under one budget can hand each call what is left. The failure classes are on the [function's page](../../reference/functions/model_infer.md).

## 8. Read the stats

`stats_output` writes, per call, what the workflow author cannot otherwise see. That is which version and digest answered, on which runtime and device, and the parameter count and artifact size the node measured at admission. It also writes `queued_ms`, `inference_ms` and `cold_load` for this call. And it writes what the manifest's own expressions charged: `ops` for all of them together, `peak_ops` for the heaviest one. A deployment running manifests it did not write sizes [`engine.ops_budget`](../../reference/configuration/engine.md#ops_budget) from `peak_ops`, because the ceiling bounds one evaluation rather than the call. A workflow that scores a competition reads `parameters` from here rather than trusting the entrant's own claim. A workflow tuning a deadline reads `inference_ms`.

## 9. Chain with `raw`

`raw: true` skips the result expression and writes every output tensor in its wire form: `{"policy": {"tensor": {"dtype": "f32", "shape": [1, 7], "data": "<base64>"}}}`. A second model's adapter reads that back with the `tensor` operator. Two graphs chain through the context without a `to_list` and a re-parse between them. The wire form is also what a trace snapshot shows.

## 10. Promote it

Export the references, and import them on the target:

```bash
orion-cli models export --status active > models.json     # references only
orion-cli models import -f models.json                    # on the target: queued for admission there
```

An export carries the reference and the manifest, never the bytes. The target instance fetches the object through its own connector of that name and admits it itself, which is why the connector travels first. Import answers as drafts, and the same `create --wait` rule applies: activate once the verdict on that node is `passed`.

## The competition walkthrough

[`examples/packages/c4-tournament`](https://github.com/GoPlasmatic/Orion/tree/main/examples/packages/c4-tournament) is a Connect Four tournament for tiny networks, and every step above in one package. It has a fixed contract every entrant's manifest must meet, and a WebAssembly plugin as the referee. A `register` channel's `model_infer` probe writes the server-measured parameter count to a leaderboard. A `turn` channel routes to whichever entrant is to move, and a `match` loop plays a game over `channel_call`. A `leaderboard` GET reads the table, and an hourly cron `round` runs the tournament. Its README walks an organizer through registering the reference entrant with `--wait` and playing the first match. The repository's e2e suite does the same against a real server.

## What the host promises, and does not

Every inference runs under the node's ceilings. Those are artifact and parameter bounds at admission, element counts in and out, a deadline, and concurrency per model and per node. An ops budget bounds the adapters. A failure of any kind writes nothing to the message. The adapters cannot read secrets or the clock, the graph sees nothing but its inputs, and on the CPU the answer is deterministic.

What the node does not promise is that the answer is right. The probe proves the graph runs and lands the declared shapes, nothing more. [Models](../../concepts/models.md) states the trust model.

## Next steps

- [Models](../../concepts/models.md): what a model is and why it is shaped this way.
- [`model_infer`](../../reference/functions/model_infer.md): the task, its fields and its failure classes.
- [Admin API › Models](../../reference/admin-api/models.md): registration, admission, activation, export.
- [Configuration › Models](../../reference/configuration/models.md): the ceilings, the runtimes, the trust keys.
- [Run the example packages](../../get-started/tutorials/examples.md): `c4-tournament`, the model-backed package.
