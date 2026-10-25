<!-- description: The v2 deployment endpoints: definitions by digest, connection revisions, plans, compare-and-swap apply, rollback, convergence and the CLI apply flow. -->
<!-- type: reference -->
<!-- last_verified: 2026-10-25 -->

# Deployment endpoints

A deployment states everything a node serves in one manifest. You store the definitions and connections it names, plan it against the current head, and apply it as a new revision. [Deployments](../../concepts/deployments.md) explains the model.

| Method | Path | Description |
|--------|------|-------------|
| POST | `/api/v2/admin/definitions` | Validate and store a workflow, channel or plugin definition; answers its digest |
| GET | `/api/v2/admin/definitions/{digest}` | Read a stored definition back |
| POST | `/api/v2/admin/definitions/test` | Run a workflow definition once on this node, without deploying it |
| GET | `/api/v2/admin/connections` | List every stored connection revision |
| POST | `/api/v2/admin/connections` | Create an immutable connection revision |
| POST | `/api/v2/admin/deployment-plans` | Check a candidate manifest against a base revision; answers the diff and the reviewed digest |
| POST | `/api/v2/admin/deployments` | Apply a manifest as the next revision, under compare-and-swap on the head |
| GET | `/api/v2/admin/deployments` | The head revision and every committed revision with its manifest |
| GET | `/api/v2/admin/deployments/{revision}` | One committed revision |
| POST | `/api/v2/admin/deployments/rollback` | Commit an earlier revision's manifest as a new revision |
| GET | `/api/v2/admin/deployments/{revision}/convergence` | Whether nodes serve a revision (`?target=this-node` or `?target=observed`) |

Once a deployment is committed, the node builds what it serves from the head manifest alone.

## Definitions

A definition is one deployable document and its kind: `{"kind": "workflow" | "channel" | "plugin", "definition": {…}}`. It holds no identity. The manifest member that names it says which entity and revision it is.

- A **workflow** is the dataflow-rs workflow document without `id`, `version`, `channel`, `status` and `rollout`. A failing task stops the run unless `on_failure` is `continue`.
- A **channel** is everything but its identity and its workflow. Its member `requires` the workflow.
- A **plugin** is its `plugin.toml` manifest as JSON and its component in base64, with an optional detached signature.

The server validates a definition the way the node loads it, then stores its canonical bytes. The answered `digest` is `sha256:` and the hex of those bytes. Two spellings of one definition, such as a defaulted field written out, are one digest. Uploading again is a no-op that answers the same digest.

```bash
curl -s -X POST http://localhost:8080/api/v2/admin/definitions \
  -H 'Content-Type: application/json' \
  -d '{"kind": "workflow", "definition": {"name": "Greet", "tasks": [
        {"id": "greet", "function": {"name": "map", "input": {"mappings": [
          {"path": "data.greeting", "logic": {"cat": ["Hello, ", {"var": "data.name"}]}}]}}}]}}'
# {"digest": "sha256:…", "kind": "workflow", "size_bytes": 214}

curl -s http://localhost:8080/api/v2/admin/definitions/sha256:3f9c… | jq '.definition.name'
```

A definition that would not serve is refused with a `400`, each problem at its field under `definition`. A workflow may call a plugin function this node does not serve yet, by a qualified name such as `acme.iso8583.parse`. Its plugin can arrive in the same deployment, so the plan checks that name, not the upload.

### Test a workflow definition

`POST /api/v2/admin/definitions/test` runs a workflow definition once on this node's engine and answers the run. The body is `{"definition": {…}, "data": {…}, "metadata": {…}}`. `definition` is the bare workflow document, without `kind`. `metadata` is optional.

- The definition is checked as an upload checks it. One that would not serve is refused with a `400` and never runs.
- The run uses this node's functions, plugins and connections. Nothing is stubbed, so a task reaches live backends: an `http_call` sends its request and a `db_write` writes. The run is recorded in the audit log (`v2_definition_test`) before it starts.
- `metadata` is normalised the way an ingress builds it, and this node's `[vars]` are stamped in.
- Nothing is stored or deployed.

```bash
curl -s -X POST http://localhost:8080/api/v2/admin/definitions/test \
  -H 'Content-Type: application/json' \
  -d '{"definition": {"name": "Greet", "tasks": [
        {"id": "greet", "function": {"name": "map", "input": {"mappings": [
          {"path": "data.greeting", "logic": {"cat": ["Hello, ", {"var": "data.name"}]}}]}}}]},
       "data": {"name": "Ada"}}'
# {"matched": true, "output": {"name": "Ada", "greeting": "Hello, Ada"},
#  "trace": {"initial": {…}, "steps": [{"task_id": "greet", "result": "executed", "changes": […]}], …},
#  "errors": []}
```

The answer always carries the trace. A run that stopped short still answers `200`, with the steps that ran and an `error` naming what stopped it. `trace.initial` is the context the run started from, and replaying each step's `changes` over it gives the context after that step.

## Connections

A connection revision is the configuration a provider runs with: `{connection_id, revision, provider_digest, public_config, secret_locators}`. `(connection_id, revision)` is immutable. Sending the same pair again with the same content answers the stored revision, and with different content a `409`.

`public_config` holds the settings. A secret-shaped key (`password`, `token`, `secret`, …), an `Authorization` value or credentials in a URL are refused there. A key naming an endpoint, ending `_url`, `_uri` or `_endpoint`, is not secret-shaped, so OAuth2's `token_url` stays public; its value is still screened for credentials. `secret_locators` maps a dotted config path to a secret reference such as `env://ORDERS_DB_URL` or `vault://secret/data/db#url`. The reference is placed at its path when the connection is used, and resolved there. A locator's path may not be one `public_config` already sets.

`provider_digest` names the provider. A built-in driver's digest is listed under `provider_backends[].package_digest` in `GET /api/v2/admin/runtime-capabilities`. The built-in drivers are `org.orion.http`, `org.orion.postgresql`, `org.orion.mysql`, `org.orion.sqlite` and `org.orion.memory-cache`.

```bash
curl -s -X POST http://localhost:8080/api/v2/admin/connections \
  -H 'Content-Type: application/json' \
  -d '{"connection_id": "orders-db", "revision": 1,
       "provider_digest": "sha256:…",
       "public_config": {"max_connections": 5, "allow_private_urls": true},
       "secret_locators": {"connection_string": "env://ORDERS_DB_URL"}}'

curl -s http://localhost:8080/api/v2/admin/connections | jq '.connections[] | {connection_id, revision}'
```

The answer carries the revision's `configuration_digest`, taken over its public settings and its locators. A manifest names the revision by it.

A connection over a built-in driver is also a connector to the built-in functions. `http_call`, `db_read`, `data_query`, `cache_read` and the rest name it by its connection id.

## Manifests

A manifest lists the members a deployment deploys:

```json
{
  "schema_version": 2,
  "providers": [],
  "members": [
    {
      "entity": { "kind": "channel", "id": "greet", "revision": 1 },
      "definition_digest": "sha256:…",
      "requires": [{ "kind": "workflow", "id": "greet-wf", "revision": 1 }]
    },
    { "entity": { "kind": "workflow", "id": "greet-wf", "revision": 1 }, "definition_digest": "sha256:…" },
    { "entity": { "kind": "connection", "id": "orders-db", "revision": 1 }, "definition_digest": "sha256:…" }
  ]
}
```

A workflow, channel or plugin member's `definition_digest` names a stored definition. A connection member's is the revision's `configuration_digest`. A channel `requires` its workflow. A workflow `requires` the connections it uses. Through `connector_call` it may call only connections it requires, and only the operations its own `connector_call` tasks name; requiring a connection grants nothing else. Every requirement must be a member of the same manifest.

## Plan and apply

A plan checks a manifest against a base revision without committing anything. It refuses a member whose definition is not stored and a connection revision that is not there. It also compiles the manifest the way the node would build it. A manifest that would not build is refused with `400 DEPLOYMENT_WOULD_NOT_BUILD`, naming why. Typical causes are a workflow calling a function no member serves, two channels claiming one route, or a workflow the engine cannot compile. It answers the added and removed entities and the `reviewed_manifest_digest`.

```bash
curl -s -X POST http://localhost:8080/api/v2/admin/deployment-plans \
  -H 'Content-Type: application/json' \
  -d '{"base_revision": 0, "manifest": '"$(cat manifest.json)"'}'
```

Apply commits the manifest as the next revision. It succeeds only while the head is still `base_revision`, so two concurrent applies cannot both win. With `reviewed_manifest_digest`, it also refuses a manifest other than the one you reviewed, with a `409`. The `idempotency_key` makes a retry replay the first answer instead of committing twice. Apply runs the plan's build check first, so a manifest that would not build is refused before anything is committed. The node then builds the revision, and the answer reports how that went under `evaluation`. Another node can still fail to build a committed revision, for example one with plugins disabled or a different `channel_filter`. The revision stays committed and `local_error` says why. That node keeps serving what it had, and its `/health` reports `engine_reload` as degraded.

```bash
curl -s -X POST http://localhost:8080/api/v2/admin/deployments \
  -H 'Content-Type: application/json' \
  -d '{"idempotency_key": "greet-1", "base_revision": 0, "reason": "deploy greet",
       "reviewed_manifest_digest": "sha256:…", "manifest": '"$(cat manifest.json)"'}'

curl -s http://localhost:8080/api/v2/admin/deployments | jq '{head_revision, applied_revision}'
curl -s http://localhost:8080/api/v2/admin/deployments/1 | jq '.manifest.members'
```

## Rollback

A rollback commits an earlier revision's manifest as a new revision. History only moves forward, so the earlier revision stays as it was.

```bash
curl -s -X POST http://localhost:8080/api/v2/admin/deployments/rollback \
  -H 'Content-Type: application/json' \
  -d '{"idempotency_key": "rollback-3", "expected_head_revision": 3, "target_revision": 1}'
```

## Convergence

Every node builds the head revision on its own. Convergence reports which nodes serve a revision: this node with `?target=this-node`, or every node the cluster has observed with `?target=observed`.

```bash
curl -s "http://localhost:8080/api/v2/admin/deployments/2/convergence?target=observed"
```

## Deploy a compiled directory

`orion-server compile` writes a definition set as a deployment directory with no revisions in it ([Deployment directories](../cli/orion-server/compile.md#deployment-directories)). `orion-cli deployments apply -d` deploys it:

```bash
orion-server compile ./definitions -o dist/deployment
orion-cli deployments plan -d dist/deployment     # what would change
orion-cli deployments apply -d dist/deployment
```

The CLI reads the target's history and does the rest:

1. It uploads every definition and checks that each answered digest is its file's name. A mismatch means the target canonicalises definitions differently from the `orion-server` that compiled the directory.
2. It assigns each member a revision. A member the head deploys with the same definition keeps its revision. A member an earlier revision deployed with the same definition takes that revision back. Anything else takes one past the highest revision that entity ever had, or 1.
3. It reuses a stored connection revision with the same provider, public settings and locators, and otherwise creates the next one. It checks each connection's provider digest against the target's built-in driver first.
4. It plans the manifest against the head and prints what moves, then applies it with the plan's reviewed digest.

The idempotency key defaults to one derived from the head and the manifest digest, so a retried apply replays. `--idempotency-key` sets it. `--base-revision` refuses to go ahead unless the head is that revision. A directory the head already deploys is not applied again. The CLI reports it as up to date.

## Related

- [`orion-server compile`](../cli/orion-server/compile.md): the command that writes a deployment directory.
- [Admin API](./index.md): every admin resource, and the contracts they share.
- [`connector_call`](../functions/connector_call.md): calling a deployed connection's provider.
