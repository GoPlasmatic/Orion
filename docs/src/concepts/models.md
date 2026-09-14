<!-- description: An ONNX graph registered by reference, admitted by the node before it serves, and run by one task function: what it is for, its lifecycle, and its limits. -->
<!-- type: concept -->
<!-- last_verified: 2026-09-14 -->

# Models

A *model* is an ONNX graph that a workflow runs inside Orion with one task function, [`model_infer`](../reference/functions/model_infer.md), on a runtime the node controls. It is a versioned entity like a [plugin](./plugins.md). It is registered and activated through the admin API, promoted between instances by reference, and served from the same runtime generation as everything else.

What sets a model apart from every other entity is that Orion never holds its bytes as the source of truth. A model row names an object in a bucket and the SHA-256 digest the bytes must hash to. No node serves an artifact it has not fetched from that bucket and hashed to the claim itself.

## What a model is for

Small networks on the hot path: a fraud score, a routing decision, a classifier over a fixed feature vector, a policy over a game board. The graph's inputs are a few thousand numbers and its weights fit in memory beside the workflow. Its answer is needed on every message, where a hop to a model server would dominate the cost.

The graph is trained elsewhere and arrives finished. Orion runs it; it does not train it. A model that needs a GPU-class budget, a batch or a token stream is a service the workflow calls with `http_call`. A transformation with no weights is a [plugin](./plugins.md) or a JSONLogic expression.

## The model

A model is an artifact and a *manifest*. The artifact is the ONNX graph. The manifest is a JSON document with `"abi": "orion:model@1.0.0"` that declares the model's name, its inputs and outputs, and the expressions that do the marshalling. Each input and output has a name, a dtype and a fixed shape. One *adapter* per input is evaluated against the JSON the task hands over, producing that input's tensor. One *result* expression is evaluated against the output tensors, producing what the task writes.

A task then names the model, the JSON root the adapters read, and where the result goes. It never spells a tensor:

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

Both kinds of expression are ordinary JSONLogic over the [tensor family](../reference/expressions.md#tensors-tensor). Its twenty operators build a tensor from JSON, reshape it, and read it back out, with deliberately no arithmetic. Compute belongs in the graph. A manifest whose caller already speaks tensors declares neither, and the defaults apply: `{"tensor": [{"var": name}, dtype]}` in, a nested list per output out.

The adapters and the result are compiled on the serving generation's own expression engine. That lets two host rules hold:

- **The budget.** Every adapter is priced by [`engine.ops_budget`](../reference/configuration/engine.md) like any other expression, and every tensor operator's cost is proportional to the data it moves. A manifest is authored by whoever owns the model, which may be a competitor, a tenant, or a team that does not run the node. The budget is what bounds an expression the operator did not write.
- **The isolation.** An adapter may not read the secret store, the clock or randomness. `{"secret": …}`, `now` and `random` are refused at registration, so a replay of a traced inference reproduces the same tensors, and a manifest cannot reach the deployment's credentials. The graph itself sees tensors and nothing else: no connectors, no context, no I/O.

On the CPU path the runtime is deterministic: the same bytes over the same inputs land the same outputs on every node. An accelerator (`metal`, `cuda`) reorders the same arithmetic and agrees with the CPU to about `f32` epsilon rather than to the bit. A deployment whose answers are scored, compared or audited should keep its whole fleet on one [device](../reference/configuration/models.md#devices). That is also the faster choice for a graph this size.

Whatever the manifest declares, the node measures. The parameter and node counts, the operators the graph asks a runtime for, the IR version and the opset are read from the graph at admission and recorded as `stats`. The parameter count is every value the graph carries, wherever it carries it — its initializers, the tensors and lists of numbers its nodes hold in attributes, the bodies of `If`, `Loop` and `Scan` — so re-exporting the same weights into a different field of the format does not change it. The [`models.max_parameters`](../reference/configuration/models.md) ceiling applies to what was read, not what was claimed.

## The entity

| Table | Key | Holds |
|---|---|---|
| `models` | `(model_id, version)` | manifest, the artifact reference (connector, key, digest), signature, tags, status, and two node-written columns: `admission` and `stats` |

There is no artifact table. The bytes live in an S3-compatible bucket behind a [`storage` connector](../reference/connectors/storage.md). The row holds the reference: the connector's name, the object key, and the `sha256:` digest. A registration carries the reference and never the bytes. A package, an export and an audit row all stay small, and the bucket stays the one place the artifact is kept.

A model follows the [entity lifecycle](./lifecycle.md) exactly: integer versions, one draft per id, active rows immutable, `draft → active → archived`. Two rules are its own:

- **One active version at a time.** Activating a draft supersedes the previously active version in the same transaction, so a model id resolves to one digest per generation.
- **A model cannot be archived or deleted while an active workflow names it by literal id.** The refusal is a `409` naming the workflows, and `GET /models/{id}/dependencies` lists them ahead of time. A workflow that routes to a model with a computed `model`, such as `{"var": "data.mover_model"}`, is outside this rule by construction: the id is not known until a message arrives, so the call fails as `unavailable` rather than being refused at activation.

### Admission

Between draft and active sits *admission*, which is what makes a model different from a plugin. A plugin upload carries its bytes and is probed before the draft exists. A model registration answers `202` at once with `admission.state = "pending"`, and the node's admission worker then does the work asynchronously:

1. Fetch the object through the connector.
2. Verify the digest, and the signature where `[models.trust]` names keys.
3. Read the graph.
4. Run five probe inferences over zero-filled inputs on the node's runtime for the manifest's format. The median must land within `models.max_probe_ms`, and the outputs must have the dtypes and shapes the manifest declares.

The verdict lands on the row as `passed`, or as `failed` with the reason and the stage it stopped at. The stages are `signature`, `gate`, `head`, `size`, `fetch`, `digest`, `cache`, `parse` and `probe`. Activation is refused until the verdict is `passed`. `orion-cli models create --wait` follows the verdict so a pipeline can stop on it. The identity of an artifact is its digest. It is claimed by the author, confirmed by every node that holds the bytes, and named by a generation, a trace and a package alike.

In a cluster, the verdict is shared and the bytes are not. Admission runs once, on the node that took the registration, and the verdict lands on the row. A peer learning of the activation through the `models` epoch scope loads the model on that verdict rather than re-running the sequence. `admission.node` keeps naming the node that did. The bytes are each node's own. A peer's artifact cache starts empty. Its first load of that model fetches the object through the storage connector and checks the digest before the graph runs. Two consequences follow. A peer that cannot reach the bucket fails at the call, not at the activation, reporting `unavailable` with the fetch stage. And the probe's `stats.probe_ms`, `runtime` and `device` describe the admitting node, which is the whole fleet's story only where the fleet is homogeneous; see [Devices](../reference/configuration/models.md#devices).

## On a node

`models.enabled = false`, the default, preserves the pre-model behaviour exactly. With it off, every model route answers `400` and a stored model row quarantines the workflows that name it rather than aborting the node.

With it on, a node keeps every verified artifact in `models.cache_dir`, one file per digest, swept least recently used under `models.max_cache_bytes`. It holds the sessions a runtime has loaded under `models.max_loaded_bytes` across every runtime. A session is keyed by the artifact **and** the binding its manifest imposes on the graph — the inputs it pins and the order it names the outputs in. So one artifact may back several models, which is how a graph serves two tenants with different adapters or two result expressions get compared without duplicating the bytes: manifests that bind the graph the same way share one session, and manifests that bind it differently are two sessions and count twice against the ceiling, because they are two plans. Which runtime runs a model is the node's decision. `[models.default_runtime]` maps an artifact format to a runtime name, and `tract` is the one this build knows. A manifest declares a format, not a runtime, and a task may name one explicitly only among the names the build compiled in. The device is the node's decision too: `cpu` by default, with `metal` and `cuda` available where the build has them. `cpu` is the right answer for almost every graph that belongs on this path.

`models.preload` decides what a generation loads before it serves. `none` means the first inference pays the load. `referenced`, the default, warms every model an active workflow names by literal id right after the generation publishes. `all` warms everything. A workflow that routes with a computed `model` names nothing for `referenced` to find, so `models.preload_tags` warms every active model carrying one of the tags it lists, whatever the mode: the tags travel on the registration and through a package, which is how an operator names a hot set the node cannot infer. `GET /models/{id}` reports this node's view under `health`. From the verdict and the lifecycle it is `pending`, `rejected`, `inactive` or `disabled`. For the active admitted version it is `admitted` until something asks for it and `loaded` while a runtime holds it. It becomes `evicted` when dropped under the ceiling, or `failed` with the reason.

A model that cannot be served on a node does not stop the node. The causes are no admitted active version, the runtime disabled, or an adapter the serving engine refuses. The workflows naming it by literal id are quarantined with the reason, and their channels answer `503` naming the model. `/health` lists it under `models.failed_to_load`, and everything else keeps serving. Every limit is the host's: a model requests nothing, and a `[[models.overrides]]` row may only lower a ceiling, never raise one.

## What it costs

tract joins the trusted computing base, and an inference is a call into it with the node's own memory and threads. The limits bound what a model can consume. They are artifact and parameter ceilings at admission, input and output element counts, and a per-call deadline capped by `models.max_timeout_ms`. Concurrency is capped per model and per node. They do not bound what it answers. The probe proves the graph runs and lands the declared shapes, not that its answers are right. The trust root is admin auth, the credential that already reads and writes connector secrets. Registering a model adds no new principal, and `[models.trust]` adds an Ed25519 signature over the digest where the bucket is not trusted on its own.

The per-request price is the adapters, the session and the result. For the 1479-parameter fixture graph through the HTTP data plane, `model_infer_test` measures 6.8 ms on a cold session and 0.7 ms warm. Under the default preload a serving node rarely takes the cold one. The task's `stats_output` field writes `inference_ms` and `queued_ms` per call. That is how a workflow author separates the graph's own time from the rest of the request. Scenario I of the [benchmark suite](https://github.com/GoPlasmatic/Orion/tree/main/crates/orion-server/tests/benchmark) drives the same graph under load.

## Next steps

- [Serve a model](../guides/extend/models.md): the manifest, the bucket, the registration and the call, end to end.
- [`model_infer`](../reference/functions/model_infer.md): the task's input fields.
- [Admin API › Models](../reference/admin-api/models.md): the routes, including `admit`.
- [Configuration › Models](../reference/configuration/models.md): the ceilings, the cache, the runtimes and the devices.
