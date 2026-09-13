<!-- description: Serve an ONNX model: write the manifest, check it offline, put the bytes in a bucket, register and admit it, and call it from a workflow with model_infer. -->
# Serve a Model

**Page type:** How-to · **Audience:** Service authors with a trained graph that belongs on the hot path

A model adds an inference to a workflow without a model server: an ONNX
graph that Orion fetches from your bucket, verifies, probes and runs on the
node, called by one task whose input and output are ordinary JSON. The
manifest does the marshalling — its adapters build the tensors, its result
expression reads them back — so the workflow never spells a tensor, and the
node measures what it serves. [Models](../concepts/models.md) is the
concept page; this one is the build.

## 1. Decide it is a model

A model is for a small graph whose answer is needed on every message: a
score over a feature vector, a classifier, a policy. Two tests, in order:

- **Does the graph fit beside the workflow?** The node loads it whole,
  under `models.max_loaded_bytes`, and runs it on the CPU by default. A
  graph that needs a GPU-class budget, batching or a token stream is a
  service you reach with `http_call`.
- **Does it have weights?** A transformation without them is a
  [plugin](./plugins.md) or a JSONLogic expression, which need no runtime.

## 2. Write the manifest

The manifest is a JSON document beside the artifact. It names the model,
declares every input and output the graph has — name, dtype, fixed shape —
and says how JSON becomes those tensors and how the outputs become JSON:

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

The **adapter** is evaluated against the JSON the task hands over and must
produce exactly the declared tensor. This one reads 42 cell values, one-hot
encodes them into `[42, 3]`, drops the "empty" plane, and turns the result
into the `[1, 2, 6, 7]` plane stack the graph was trained on — four
[tensor operators](../reference/expressions.md#tensors-tensor), each priced
by the data it moves. The **result** is evaluated against the outputs by
name and produces what the task writes: here the index of the largest
policy entry, which for a `[1, 7]` tensor along axis 1 is a one-element
list. Leave either out and the default applies — `{"tensor": [{"var":
name}, dtype]}` for an input, a nested list for an output — which is enough
for a caller that already speaks tensors.

`name` is the model id: lowercase labels joined by `.`, `orion.*` reserved.
`version` is yours and informational, Orion assigns the entity version.
`artifact` is the file beside the manifest, read by offline tooling and the
CLI only — a registration names the bytes by bucket, key and digest. An
adapter may not read `{"secret": …}`, `now` or `random`: a replay must
reproduce the same tensors, and a manifest's author is not the secrets'
owner.

## 3. Validate it offline

```bash
orion-server lint ./definitions --model-dir ./entrant
orion-server test ./cases --plugin-dir ./plugin --model-dir ./entrant
```

`--model-dir` names a directory holding a manifest and the artifact it
names, and gives the offline runners the model: `lint` checks a
`model_infer` task's literal `model` against a manifest it can see and the
manifest against the graph beside it, and `test` and `dry-run` run the
inference for real rather than stubbing it — a case naming a model the
directory does not hold fails as `MODEL_ARTIFACT_UNAVAILABLE`, never on a
silently passing stub. Without it the function is stubbed like a connector
— a `"model_infer": {"*": …}` entry in the stubs file is what the task
writes — which is how a workflow around a model is tested before the model
exists. No admission runs offline: the digest is computed from the file,
and the bytes are trusted as your own. [Test Workflows Offline](./testing.md)
covers the case format.

## 4. Put the bytes in the bucket

Any S3-compatible client will do; Orion's part is a
[`storage` connector](../reference/connectors.md#storage) that can read the
object — `operations.presign_get` on, which is the default:

```bash
aws s3 cp entrant/c4-tiny.onnx s3://models/c4-tiny.onnx
orion-cli connectors create -d '{"name":"models","connector_type":"storage","config":{
  "type":"storage","endpoint":"https://s3.eu-west-1.amazonaws.com","region":"eu-west-1",
  "bucket":"models","access_key":"env://MODELS_ACCESS_KEY","secret_key":"env://MODELS_SECRET_KEY"}}'
```

The node fetches through this connector at admission, and again on any node
that has never seen the digest, so the connector has to exist on every
instance the model is promoted to.

## 5. Register it, and wait

```bash
orion-cli models create -f entrant/model.json --connector models --key c4-tiny.onnx \
  --digest "sha256:$(shasum -a 256 entrant/c4-tiny.onnx | cut -d' ' -f1)" --wait
```

The registration carries the manifest and the reference, never the bytes,
and answers at once with `admission.state = "pending"`. The node then
fetches the object, confirms the digest, reads the graph and runs the probe;
`--wait` follows the verdict, printing each state change, and exits `0` on
`passed`, `1` on `failed` with the stage and the reason, `2` if `--timeout`
elapses first — so the same command gates a pipeline.
`orion-cli models get <id>` shows the verdict and the stats the node
measured (`parameters`, the opset, the artifact size), and
`orion-cli models admit <id> --wait` runs admission again after a fix.

## 6. Activate it

```bash
orion-cli models activate example.c4-tiny
```

Refused with a `409` until admission has passed. Activation rebuilds the
engine; under the default `models.preload = "referenced"` the node warms the
session for every active workflow that names the model, so the first
request rarely finds it cold. The server needs `models.enabled = true` and
a `cache_dir`, and `--signature` where it configures `[models.trust]` — see
[Configuration › Models](../reference/configuration.md#models).

## 7. Call it from a workflow

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

`input` is the JSON root every adapter reads — `{"var": ""}` hands over the
whole context, a path hands over one object. A literal `model` is what the
dependency list, the quarantine and the preload see; a computed one
(`{"var": "data.mover_model"}`) routes per message and is checked per
message instead. `timeout_ms` is the call's deadline, capped by
`models.max_timeout_ms`, and a cold load is charged to it. The failure
classes — the adapter produced the wrong tensor, the model is unavailable
on this node, the deadline elapsed — are on the
[function's page](../reference/functions.md#model_infer).

## 8. Read the stats

`stats_output` writes, per call, what the workflow author cannot otherwise
see: which version and digest answered, on which runtime and device, the
parameter count and artifact size the node measured at admission, and
`queued_ms`, `inference_ms` and `cold_load` for this call. A workflow that
scores a competition reads `parameters` from here rather than trusting the
entrant's own claim; a workflow tuning a deadline reads `inference_ms`.

## 9. Chain with `raw`

`raw: true` skips the result expression and writes every output tensor in
its wire form, `{"policy": {"tensor": {"dtype": "f32", "shape": [1, 7],
"data": "<base64>"}}}`. A second model's adapter reads that back with the
`tensor` operator, so two graphs chain through the context without a
`to_list` and a re-parse between them — and the wire form is also what a
trace snapshot shows.

## 10. Promote it

```bash
orion-cli models export --status active > models.json     # references only
orion-cli models import -f models.json                    # on the target: queued for admission there
```

An export carries the reference and the manifest, never the bytes; the
target instance fetches the object through its own connector of that name
and admits it itself, which is why the connector travels first. Import
answers as drafts, and the same `create --wait` rule applies: activate once
the verdict on that node is `passed`.

## The competition walkthrough

[`examples/packages/c4-tournament`](https://github.com/GoPlasmatic/Orion/tree/main/examples/packages/c4-tournament)
is a Connect Four tournament for tiny networks, and every step above in one
package: a fixed contract every entrant's manifest must meet, a
WebAssembly plugin as the referee, a `register` channel whose `model_infer`
probe writes the server-measured parameter count to a leaderboard, a
`turn` channel that routes to whichever entrant is to move, a `match` loop
over `channel_call`, a `leaderboard` GET, and an hourly cron `round`. Its
README walks an organiser through registering the reference entrant with
`--wait` and playing the first match; the repository's e2e suite does the
same against a real server.

## What the host promises, and does not

Every inference runs under the node's ceilings — artifact and parameter
bounds at admission, element counts in and out, a deadline, concurrency per
model and per node, an ops budget over the adapters — and a failure of any
kind writes nothing to the message. The adapters cannot read secrets or the
clock, the graph sees nothing but its inputs, and on the CPU the answer is
deterministic. What the node does **not** promise is that the answer is
right: the probe proves the graph runs and lands the declared shapes,
nothing more. [Models](../concepts/models.md) states the trust model.

## Related

- [Models](../concepts/models.md): what a model is and why it is shaped this way.
- [`model_infer`](../reference/functions.md#model_infer): the task, its fields and its failure classes.
- [Admin API › Models](../reference/admin-api.md#models): registration, admission, activation, export.
- [Configuration › Models](../reference/configuration.md#models): the ceilings, the runtimes, the trust keys.
- [Run the Examples](../getting-started/examples.md): `c4-tournament`, the model-backed package.
