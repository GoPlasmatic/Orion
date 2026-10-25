<!-- description: orion-server compile resolves $from, use, $each and $sql in a definition set and writes it as a revision-free v2 deployment directory. -->
<!-- type: reference -->
<!-- last_verified: 2026-10-25 -->

# `orion-server compile`

Compiles a definition set into a deployment directory, resolving its authoring conveniences: `$from`, `use`, `$use`, `$each` and `$sql`. Needs no config, database, or server.

## Synopsis

```bash
orion-server compile <dir> -o <DIR> [--format deployment] [--deny-warnings] [--plugin-dir <DIR>]...
```

## Description

**Why it exists.** References resolve when a *set* is loaded, and the admin API loads no set: it takes one document, with nothing to resolve names against. Without this step the only path from `definitions/` to a running instance was a deploy tool that reimplemented the expander. A partial reimplementation shows up as `UNCOMPILED_SOURCE` on the upload, 62 workflows deep.

**It runs `lint <dir>` first**, and writes nothing if that fails. A deployment carries everything it references, so every reference must resolve inside the set.

**A `plugin.toml` in the set compiles into the deployment.** Its component, the file the manifest's `component` names beside it, is inlined as base64, so the deployment installs the plugin on a target that has never seen it.

Every entity must carry an explicit id. A member of a deployment is its kind and its id, and the target matches a recompiled member to what it already deploys by them.

## Options

| Flag | Description |
|------|-------------|
| `-o, --output` | The directory to write. Required. |
| `--format` | `deployment`, the default and only format. |
| `--deny-warnings` | Exit non-zero on advisory findings too, not only errors. |
| `--plugin-dir` | Directory of plugin manifests beyond the set's own tree. Repeatable. |

## Deployment directories

`compile` writes the set as a v2 deployment with no revisions in it. Only the target knows which revision a changed definition gets, so the deployer assigns them. [`orion-cli deployments apply -d`](../../admin-api/deployments.md#deploy-a-compiled-directory) does.

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

The same set compiles to the same bytes. A recompile into the same directory removes the files under `definitions/` and `connections/` that the set no longer produces.

## Examples

```bash
orion-server compile ./definitions -o dist/deployment
orion-cli deployments plan -d dist/deployment
orion-cli deployments apply -d dist/deployment
```

```bash
orion-server compile ./definitions -o dist/deployment && orion-cli --server https://prod.orion.internal deployments apply -d dist/deployment
```

## Related

- [Shared definitions](../shared-definitions.md): the conveniences this command resolves.
- [Deployment endpoints](../../admin-api/deployments.md#deploy-a-compiled-directory): how `orion-cli deployments apply -d` deploys the directory.
- [CI/CD with deployments](../../../guides/patterns/ci-cd.md): compile in a pipeline, from a repository to an instance.
- [`orion-server` commands](./index.md): every `orion-server` subcommand.
