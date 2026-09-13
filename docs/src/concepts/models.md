<!-- description: A model is an ONNX graph registered by reference, admitted by every node that serves it, and run by one task function: what it can and cannot do, its lifecycle, and when to use one. -->
# Models

A model is an ONNX graph that a workflow runs inside Orion with one task
function, [`model_infer`](../reference/functions.md#model_infer), on a
runtime the node controls. It is a versioned entity like a
[plugin](./plugins.md): registered and activated through the admin API,
promoted between instances by reference, synced across a cluster by the
config epoch, and served from the same runtime generation as everything
else. What sets it apart from every other entity is that Orion never holds
its bytes as the source of truth. A model row names an object in a bucket
and the SHA-256 digest the bytes must hash to, and each node fetches,
verifies and probes the artifact for itself before it will serve it.

## What a model is for

Small networks on the hot path: a fraud score, a routing decision, a
classifier over a fixed feature vector, a policy over a game board — a graph
whose inputs are a few thousand numbers, whose weights fit in memory beside
the workflow, and whose answer is needed on every message where a hop to a
model server would dominate the cost. The graph is trained elsewhere and
arrives finished; Orion runs it, it does not train it. A model that needs a
GPU-class budget, a batch, or a token stream is a service the workflow calls
with `http_call`, and a transformation with no weights is a
[plugin](./plugins.md) or a JSONLogic expression.

## The model

A model is an artifact and a **manifest**. The artifact is the ONNX graph.
The manifest is a JSON document — `"abi": "orion:model@1.0.0"` — that
declares the model's name, its inputs and outputs (each a name, a dtype and a
fixed shape), and two kinds of expression that do the marshalling: one
**adapter** per input, evaluated against the JSON the task hands over to
produce that input's tensor, and one **result** expression, evaluated
against the output tensors to produce what the task writes. A task then
names the model, the JSON root the adapters read, and where the result goes,
and never spells a tensor:

```json
{
  "name": "model_infer",
  "input": {
    "model": "example.c4-tiny",
    "input": { "var": "temp_data.view" },
    "output": "data.answer",
    "stats_output": "data.inference"
  }
}
```

Both kinds of expression are ordinary JSONLogic over the
[tensor family](../reference/expressions.md#tensors-tensor): twenty
operators that build a tensor from JSON, reshape it, and read it back out,
with deliberately no arithmetic — compute belongs in the graph. A manifest
whose caller already speaks tensors declares neither, and the defaults apply
(`{"tensor": [{"var": name}, dtype]}` in, a nested list per output out).
The adapters and the result are compiled on the serving generation's own
expression engine, which is what lets two host rules hold:

- **The budget.** Every adapter is priced by
  [`engine.ops_budget`](../reference/configuration.md#engine) like any
  other expression, and every tensor operator's cost is proportional to the
  data it moves. A manifest is authored by whoever owns the model — a
  competitor, a tenant, a team that does not run the node — and the budget
  is what bounds an expression the operator did not write.
- **The isolation.** An adapter may not read the secret store, the clock or
  randomness: `{"secret": …}`, `now` and `random` are refused at
  registration, so a replay of a traced inference reproduces the same
  tensors and a manifest cannot reach the deployment's credentials. The
  graph itself sees tensors and nothing else — no connectors, no context,
  no I/O — and on the CPU path the runtime is deterministic: the same bytes
  over the same inputs land the same outputs on every node.

Whatever the manifest declares, the node measures: the parameter and node
counts, the IR version and opset are read from the graph at admission and
recorded as `stats`, and the [`models.max_parameters`](../reference/configuration.md#models)
ceiling applies to what was read, not what was claimed.

## The entity

| Table | Key | Holds |
|---|---|---|
| `models` | `(model_id, version)` | manifest, the artifact reference (connector, key, digest), signature, tags, status — and two derived columns, `admission` and `stats`, written by the node |

There is no artifact table. The bytes live in an S3-compatible bucket behind
a [`storage` connector](../reference/connectors.md#storage), and the row
holds the reference: the connector's name, the object key, and the
`sha256:` digest. A registration carries the reference and never the bytes,
so a package, an export and an audit row all stay small, and the bucket
stays the one place the artifact is kept.

A model follows the [entity lifecycle](./lifecycle.md) exactly: integer
versions, one draft per id, active rows immutable, `draft → active →
archived`. Two rules are its own:

- Exactly one version of a model is active at a time. Activating a draft
  supersedes the previously active version in the same transaction, so a
  model id resolves to one digest per generation.
- A model cannot be archived or deleted while an active workflow names it
  by literal id; the refusal is a `409` naming the workflows, and
  `GET /models/{id}/dependencies` lists them ahead of time. A workflow that
  routes to a model with a computed `model` — `{"var": "data.mover_model"}`
  — is outside this rule by construction, because the id is not known
  until a message arrives; the call then fails as `unavailable` rather
  than being refused at activation.

Between the two states sits **admission**, which is what makes a model
different from a plugin. A plugin upload carries its bytes and is probed
before the draft exists. A model registration answers `202` at once with
`admission.state = "pending"`, and the node's admission worker then does the
work asynchronously: fetch the object through the connector, verify the
digest (and the signature, where `[models.trust]` names keys), read the
graph, and run five probe inferences over zero-filled inputs on the node's
runtime for the manifest's format — the median must land within
`models.max_probe_ms`, and the outputs must have the dtypes and shapes the
manifest declares. The verdict lands on the row as `passed`, or `failed`
with the stage it stopped at (`signature`, `gate`, `head`, `size`, `fetch`,
`digest`, `cache`, `parse`, `probe`) and the reason. **Activation is refused
until the verdict is `passed`**, and `orion-cli models create --wait`
follows the verdict so a pipeline can stop on it. The identity of an
artifact is its digest: claimed by the author, confirmed by every node that
admits it, and what a generation, a trace and a package all name.

## On a node

`models.enabled = false` (the default) preserves the pre-model behaviour
exactly; with it off, every model route answers `400` and a stored model
row quarantines the workflows that name it rather than aborting the node.
With it on, a node keeps every verified artifact in `models.cache_dir`, one
file per digest, swept least-recently-used under `models.max_cache_bytes`,
and holds the sessions a runtime has loaded under `models.max_loaded_bytes`
across every runtime. Which runtime runs a model is the node's decision —
`[models.default_runtime]` maps an artifact format to a runtime name, and
`tract` is the one this build knows — so a manifest declares a format, not
a runtime, and a task may name one explicitly only among the names the
build compiled in.

`models.preload` decides what a generation loads before it serves: `none`
(the first inference pays the load), `referenced` — the default: every
model an active workflow names by literal id is warmed right after the
generation publishes — or `all`. `GET /models/{id}` reports this node's
view under `health`: `pending`, `rejected`, `inactive` or `disabled` from
the verdict and the lifecycle; for the active admitted version `admitted`
until something asks for it, `loaded` (with the runtime, the device and the
resident bytes) while a runtime holds it, `evicted` when it was held and
dropped under the ceiling, or `failed` with the reason.

A model that cannot be served on a node — no admitted active version, the
runtime disabled, an adapter the serving engine refuses — does not stop the
node. The workflows naming it by literal id are quarantined with the
reason, their channels answer `503` naming the model, `/health` lists it
under `models.failed_to_load`, and everything else keeps serving. Every
limit is the host's: a model requests nothing, and a `[[models.overrides]]`
row may only lower a ceiling, never raise one.

## What it costs

tract joins the trusted computing base, and an inference is a call into it
with the node's own memory and threads. The limits bound what a model can
consume — artifact and parameter ceilings at admission, input and output
element counts, a per-call deadline capped by `models.max_timeout_ms`,
concurrency per model and per node — but not what it answers: the probe
proves the graph runs and lands the declared shapes, not that its answers
are right. The trust root is admin auth, the credential that already reads
and writes connector secrets; registering a model adds no new principal,
and `[models.trust]` adds an Ed25519 signature over the digest where the
bucket is not trusted on its own.

The per-request price is the adapters, the session and the result. For the
1479-parameter fixture graph through the HTTP data plane, `model_infer_test`
measures **6.8 ms for the first call on a cold session and 0.7 ms warm**,
and under the default preload a serving node rarely takes the cold one. The
`stats_output` field of the task writes `inference_ms` and `queued_ms` per
call, which is how a workflow author separates the graph's own time from
the rest of the request, and scenario I of the
[benchmark suite](https://github.com/GoPlasmatic/Orion/tree/main/crates/orion-server/tests/benchmark)
drives the same graph under load.

See [Serve a Model](../build/models.md) for the build — the manifest, the
bucket, the registration, the call — and the
[`model_infer`](../reference/functions.md#model_infer),
[Admin API › Models](../reference/admin-api.md#models) and
[Configuration › Models](../reference/configuration.md#models) references
for the task, the routes and the ceilings.
