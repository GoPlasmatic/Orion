<!-- description: The model endpoints: registering an ONNX model by bucket reference and digest, the admission verb, and why activation waits for a verdict. -->
<!-- type: reference -->
<!-- last_verified: 2026-09-14 -->

# Model endpoints

The endpoints that register, admit, version and activate an ONNX model held in object storage.

ONNX models held in object storage. The lifecycle is the workflow's. A registration carries the manifest, which is the `orion:model@1.0.0` JSON document, and an **artifact reference**. The reference is a `storage` connector, an object key and the `sha256:` digest the bytes must hash to. It never carries the bytes. The manifest itself may name where its bytes are twice, for the two places it is read. `artifact` is a path relative to the manifest, read only by the offline tooling. `lint` reports the graph's stats from it, `dry-run` and `test` run the model from it, and `compile` hashes it. `reference` is `{ "connector", "key" }`, where a pipeline put the same bytes for a serving instance. [`compile`](../cli/orion-server/compile.md) writes that into a package's `models[]` entry, beside the digest of the local file. A served row ignores both — the request's `artifact` reference is its authority — and neither is required. The server validates the manifest and checks the connector exists and allows reads (`operations.presign_get`). It confirms the object is there and within `models.max_artifact_bytes`, writes the draft, and answers `202`. The node's admission worker then fetches the object through the connector and verifies the digest.

It reads the graph: parameter and node counts, the distinct operators it asks a runtime for, IR version and opset. The parameter count is every value the graph carries, in its initializers or in its nodes' attributes, through every subgraph body. Every manifest input and output must name a graph tensor, and `models.max_parameters` applies.

It then probes the model on the node's default runtime for the manifest's format (`[models.default_runtime]`). The probe is five inferences over zero-filled inputs. The median must be within `models.max_probe_ms`, and the outputs must have the dtypes and shapes the manifest declares.

The verdict lands on the row as `admission` (`pending` → `passed` | `failed`) with a stage and a reason, plus `stats`. The stages are `signature`, `gate`, `head`, `size`, `fetch`, `digest`, `cache`, `parse` and `probe`. Activation is refused until the verdict is `passed`. Every route answers `400` on a node with `models.enabled = false`.

| Method | Path | Description |
|--------|------|-------------|
| POST | `/api/v1/admin/models` | Register a model as a draft: `{ "manifest": {…}, "artifact": { "connector", "key", "digest" }, "signature": "<base64>", "tags": [] }`. `202` with the row, `admission.state` `pending` and the draft queued for admission on this node. `signature` is required when `[models.trust]` names keys. `400` on a bad manifest, reference, signature or connector |
| GET | `/api/v1/admin/models` | List models. Filter with `?tag=`, `?status=`, `?admission=pending\|passed\|failed` |
| GET | `/api/v1/admin/models/{id}` | Get the latest version, with this node's view under `health`: `pending`, `rejected`, `inactive` or `disabled` from the verdict and the lifecycle; for the active admitted version `admitted` until something asks for it, `loaded` (with `runtime`, `device`, `resident_bytes`) while a runtime holds it, `evicted` when it was held and dropped under `models.max_loaded_bytes`, or `failed` (with the reason) when this node's generation could not carry it |
| PUT | `/api/v1/admin/models/{id}` | Update the draft: any of `manifest`, `artifact`, `signature`, `tags`; an absent field keeps its value. A changed artifact reference resets `admission` to `pending` and queues the draft again; a manifest or tag change alone keeps the verdict |
| DELETE | `/api/v1/admin/models/{id}` | Delete every version. `409` while an active workflow names the model |
| PATCH | `/api/v1/admin/models/{id}/status` | Activate (supersedes the previously active version; `409` until admission has passed) or archive (`409` while an active workflow names the model). `?dry_run=true` / `?reload=defer` — see [Status changes](./status-changes.md) |
| POST | `/api/v1/admin/models/{id}/admit` | Run admission for the latest version on this node again: `202` with the row and the job queued, or `?wait=true` for `200` with the verdict recorded. Idempotent |
| GET | `/api/v1/admin/models/{id}/versions` | Version history |
| POST | `/api/v1/admin/models/{id}/versions` | New draft version from the latest — the verdict and stats come with it, because the reference is unchanged |
| GET | `/api/v1/admin/models/{id}/dependencies` | The active workflows calling `model_infer` on this model by its literal id, with the task ids; `dynamic_references_unlisted` says a computed `model` is not seen |
| POST | `/api/v1/admin/models/import` | Bulk import (as drafts). Items carry the manifest and the reference, never bytes; each item written is queued for admission on this node. `?dry_run=true`, `?on_conflict=fail\|skip\|new_version` |
| GET | `/api/v1/admin/models/export` | Export models — references only, importable as they are. Filter with `?tag=`, `?status=` |
| POST | `/api/v1/admin/models/validate` | Validate a registration without storing it; `valid: true` means `POST /models` would accept the payload on this node, and `head` carries the object's size and ETag |

```bash
# Register from a manifest, naming the object in the `models` storage connector
jq -n --slurpfile manifest model.json \
  --arg digest "sha256:$(sha256sum c4-tiny.onnx | cut -d' ' -f1)" \
  '{manifest: $manifest[0], artifact: {connector: "models", key: "c4-tiny.onnx", digest: $digest}}' \
  | curl -s -X POST http://localhost:8080/api/v1/admin/models \
      -H 'Content-Type: application/json' --data @-

# Poll for the verdict: admission.state is pending, then passed or failed
curl -s http://localhost:8080/api/v1/admin/models/ada.c4-tiny | jq '.data.admission'

# Activate once it has passed; the engine reloads
curl -s -X PATCH http://localhost:8080/api/v1/admin/models/ada.c4-tiny/status \
  -H 'Content-Type: application/json' -d '{"status":"active"}'

# Export the references, for a promotion — the target fetches the bytes itself
curl -s "http://localhost:8080/api/v1/admin/models/export?tag=vision" | jq '.data' > models.json
```

## Related

- [Admin API](./index.md): every admin resource, and the contracts they share.
- [Models](../../concepts/models.md): what a model is, and what it is for.
- [Model settings](../configuration/models.md): the `[models]` block a node needs before any of this works.
- [Serve a model](../../guides/extend/models.md): the same endpoints, as a walkthrough.
