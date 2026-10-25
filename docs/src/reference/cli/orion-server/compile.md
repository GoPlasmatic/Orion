<!-- description: orion-server compile resolves $from and use in a definition set and writes a package artifact, a directory of request bodies, bulk-import files, or a v2 deployment directory. -->
<!-- type: reference -->
<!-- last_verified: 2026-10-25 -->

# `orion-server compile`

Compiles a definition set into files the admin API accepts, resolving its authoring conveniences: `$from`, `use`, `$use`, `$each` and `$sql`. Needs no config, database, or server.

## Synopsis

```bash
orion-server compile <dir> [-o <PATH>] [--format artifact|dir|bulk|deployment]
                           [--name NAME] [--version VERSION|content [--version-prefix PREFIX]]
                           [--requires-channel NAME]... [--requires-connector NAME]...
                           [--deny-warnings] [--no-activate]
```

## Description

**A model manifest in the set compiles into the artifact.** Each becomes a `models[]` entry in the shape `POST /models/import` accepts. The entry holds the manifest (minus its local `artifact` path), an `artifact` reference, and `tags`, marked `activate: true` like everything else. The reference takes `connector` and `key` from the manifest's `reference`, and a `digest` computed from the file its `artifact` names. Both halves are required for `--format artifact`, and a manifest missing either is refused naming what to add. An artifact must name bytes the target can reach, and the digest is what the target checks them against at admission. The storage connectors those references name go to `requires.storage` unless the set carries a connector of that name. `--format bulk` writes the same entries to `models.json`, to be sent after `connectors.json`.

**A `plugin.toml` in the set compiles into the artifact.** Its component, the file the manifest's `component` names beside it, is inlined as base64 under `plugins[]`, digest included. The artifact therefore installs the plugin on a target that has never seen it, and activates it before any workflow that calls it. A manifest with no component beside it is refused for `--format artifact`. The artifact is what carries the bytes, and one without them would fail at `apply`. `--format bulk` writes the same entries to `plugins.json`.

**Why it exists.** References resolve when a *set* is loaded, and the admin API loads no set: it takes one document, with nothing to resolve names against. Without this step the only path from `definitions/` to a running instance was a deploy tool that reimplemented the expander. A partial reimplementation shows up as `UNCOMPILED_SOURCE` on the POST, 62 workflows deep.

**It runs `lint <dir>` first**, and emits nothing if that fails. A compile that wrote out a set its own linter rejects is how an artifact reaches `package apply` having passed CI.

`artifact` marks workflows and channels `activate: true`, because a directory carries no stored status and a package whose entities never activate applies cleanly and serves nothing. Set `"activate": false` on an entity, or pass `--no-activate`, to override. `dir` and `bulk` emit no activation intent — that is a package concept, and their files are request bodies.

Entities must carry explicit ids for `artifact` and `deployment` only. `apply` activates a channel by `channel_id` and reads activation intent off it. An id-less entity in an artifact is one `apply` would stage and never activate. `dir` and `bulk` emit request bodies, where the server derives an id from the name exactly as it does for a hand-written POST. Leaving `channel_id` out of a definition is an ordinary way to author a set. Committing a server-generated UUID would tie the set to one instance.

## Options

| Flag | Description |
|------|-------------|
| `-o, --output` | A file for `--format artifact` (default: stdout); a directory for `dir`, `bulk` and `deployment`, where it is required. |
| `--format` | `artifact` (default), `dir`, `bulk`, or `deployment`; each is described under [Output formats](#output-formats). |
| `--name` | Package name. Required for `--format artifact` unless the set declares `package.name`; the flag wins, with a note, when both are given. |
| `--version` | Package version. Required for `--format artifact`. Applied versions are immutable — any content change needs a bump. `content` derives the version from the content hash instead; see [Content versions](#content-versions). A version the target would refuse (outside letters, digits, `.`, `_` and `-`, or over 64 characters) is refused here. |
| `--version-prefix` | With `--version content`, writes `<PREFIX>-<12 hex>` instead of `content-<12 hex>`. At most 51 characters. |
| `--requires-orion <range>` | The Orion version range the artifact requires of a target, written to `requires.orion`. Overrides the set's own [`package.requires.orion`](../shared-definitions.md#the-package-document). |
| `--signatures <dir>` | Write the detached signatures in `<dir>` into the artifact's `plugins[]` and `models[]` entries, for a signer that runs at build time. The files are named as [`package apply --signatures`](./package.md#signatures-at-deploy-time) reads them, and a model's local artifact file name also matches. The hash and a content version do not move. |
| `--requires-channel` | Channel name that may be referenced without being in the set; recorded in the artifact's `requires`. Repeatable. |
| `--requires-connector` | Connector name that may be referenced without being in the set. Repeatable. |
| `--deny-warnings` | Exit non-zero on advisory findings too, not only errors. |
| `--no-activate` | Do not mark workflows and channels for activation, so the artifact applies as drafts. |
| `--plugin-dir` | Directory of plugin manifests beyond the set's own tree. Repeatable. |
| `--model-dir` | Directory of model manifests beyond the set's own tree. Repeatable. |

### Output formats

| `--format` | Output | Consumed by |
|---|---|---|
| `artifact` | One promotion artifact, hashed exactly as `package export` hashes one | `orion-server package plan\|apply\|diff` |
| `dir` | The input tree mirrored, one file per entity, shared documents consumed | a POST per file — `orion-cli workflows import -f …` |
| `bulk` | `connectors.json`, `workflows.json`, `channels.json` — plus `plugins.json` and `models.json` when the set carries any | the bulk import endpoints, in that order (plugins first, models after connectors) |
| `deployment` | A revision-free v2 deployment directory; see [Deployment directories](#deployment-directories) | `orion-cli deployments plan\|apply -d` |

### Deployment directories

`--format deployment` writes the set as a v2 deployment with no revisions in it. Only the target knows which revision a changed definition gets, so the deployer assigns them. [`orion-cli deployments apply -d`](../../admin-api/deployments.md#deploy-a-compiled-directory) does.

```text
dist/
├── deployment.json
├── definitions/
│   ├── 87bf52da…a3f0.json
│   └── e627767c…be874.json
└── connections/
    └── orders-db.json
```

`deployment.json` lists the members, sorted by kind and then id:

```json
{
  "schema_version": 1,
  "members": [
    {
      "kind": "channel",
      "id": "order-tiers",
      "definition": "sha256:e627767c…",
      "requires": [{ "kind": "workflow", "id": "order-classification" }]
    },
    { "kind": "connection", "id": "orders-db", "connection": "orders-db", "requires": [] },
    { "kind": "workflow", "id": "order-classification", "definition": "sha256:87bf52da…", "requires": [] }
  ]
}
```

**A workflow, channel or plugin is a definition file.** Each holds one `{kind, definition}` document in the canonical bytes `POST /api/v2/admin/definitions` stores, named by the hex of its digest. Uploading it answers that digest. A workflow loses `workflow_id`, `version` and `status`, which the member supplies, and `continue_on_error: true` becomes `on_failure: "continue"`. A channel loses `channel_id`, `version`, `status` and `workflow_id`. Its workflow becomes the member's `requires`. A plugin is its manifest and its component, inlined as base64. A key a definition does not define is an error naming the entity and the key, never dropped. That includes a channel's `topic` and `"activate": false`, because every member of a deployment serves.

**A connector is a connection file.** `connections/<name>.json` holds `{connection_id, driver, provider_digest, public_config, secret_locators}`. The connector's `name` is the connection id, so tasks name it as before. The driver comes from the connector's type:

| Connector | Driver |
|---|---|
| `http` | `org.orion.http` |
| `db`, `"driver": "postgresql"` | `org.orion.postgresql` |
| `db`, `"driver": "mysql"` | `org.orion.mysql` |
| `db`, `"driver": "sqlite"` | `org.orion.sqlite` |
| `cache`, backend `memory` | `org.orion.memory-cache` |

Any other connector is an error. A `db` connector's driver comes from its [`driver`](../../connectors/db.md#the-driver-field) field, not its connection string, so a `connection_string` that is a secret reference compiles. Every `db` connector in a set names one (`[connector.db_driver]`). The field becomes the provider and does not appear in `public_config`. `provider_digest` is the driver's built-in digest, the one `GET /api/v2/admin/runtime-capabilities` reports.

Every string in the config that is a secret reference (`env://`, `vault://`) moves to `secret_locators` at its dotted path, such as `auth.token`. The rest is `public_config`, without the `type` key. A deployment carries references only. A literal credential, such as a secret-shaped key with a literal value or a password in a URL, is an error.

A connector's tags are not carried, and compile prints a note saying so. A workflow `requires` every connection its tasks name: the `connector` of a task whose function takes one (`http_call`, `db_read`, `db_write`, `data_query`, `data_write`, the `cache_*` functions) and the `connection` of a `connector_call`, inside task groups and `loop.setup` too. Each must be a connector in the set. A requirement grants nothing by itself: a workflow may call through `connector_call` only the operations its own `connector_call` tasks name.

The same set compiles to the same bytes. A recompile into the same directory removes the files under `definitions/` and `connections/` that the set no longer produces. `--requires-channel` and `--requires-connector` do not apply: a deployment carries everything it references. A model manifest in the set is an error, because a deployment cannot carry one yet.

### Content versions

`--version content` names the package after its own content hash: `content-` and the first 12 hex of `content_hash`. The version therefore moves exactly when `plan`, `apply` and `diff` would see a change. A rebuild of unchanged definitions compiles to the version already applied. A deploy that runs on every image build needs no version scheme of its own.

```console
$ orion-server compile ./definitions --name orders --version content -o dist/orders.json
wrote orders@content-3f9c2a1b7d04 (4 connectors, 53 workflows, 55 channels) to dist/orders.json
```

`--version-prefix 1.4.0` writes `1.4.0-3f9c2a1b7d04` instead. Package versions are opaque labels: a target decides `current` by when a version was applied, never by comparing versions, so nothing may assume these order. [`package lint`](./package.md) checks that a `content-<12 hex>` version names the content the artifact hashes to.

**What a content version does not see.** The hash covers importable content only. A change to `activate`, `--no-activate`, a workflow's `rollout_percentage`, `requires` or a signature keeps the version. Applying such an artifact again reports the version as applied with identical content, and does nothing. `compile` prints a note when a workflow carries `rollout_percentage`. Change `--version-prefix` to force a new version.

A revert is a rollback. Reverting the definitions compiles to the earlier version, whose receipt a later version superseded, and `apply` puts that content back.

## Examples

```
$ orion-server compile ./definitions --name payments --version 1.4.0 -o dist/package.json
compiled: shared.fragments rewrote 23 document(s)
compiled: shared.values rewrote 51 document(s)
compiled: shared.sql rewrote 12 document(s)
./definitions: 4 connector(s), 62 workflow(s), 62 channel(s), 9 shared value(s), 3 fragment(s) — 0 error(s), 0 warning(s)
wrote payments@1.4.0 (4 connectors, 62 workflows, 62 channels) to dist/package.json
```

```bash
orion-server compile ./definitions --name payments --version 1.4.0 -o dist/package.json && orion-server package apply -s https://prod.orion.internal -f dist/package.json
```

```bash
orion-server compile ./definitions --format deployment -o dist/deployment && orion-cli --server https://prod.orion.internal deployments apply -d dist/deployment
```

## Related

- [Shared definitions](../shared-definitions.md): the conveniences this command resolves.
- [Packages](../../../concepts/packages.md): what a promotion artifact is.
- [CI/CD with packages](../../../guides/patterns/ci-cd.md): compile in a pipeline, from a repository to an instance.
- [`orion-server package`](./package.md): the verbs that consume the artifact.
- [Deployment endpoints](../../admin-api/deployments.md): what a deployment directory deploys through.
- [`orion-server` commands](./index.md): every `orion-server` subcommand.
