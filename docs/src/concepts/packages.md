<!-- description: A package is one Orion service versioned as a unit — its channels, workflows, connectors, plugins and models — and the boundary along which a service ships. -->
<!-- type: concept -->
<!-- last_verified: 2026-09-19 -->

# Packages

A *package* is one service, named and versioned as a unit. It holds the channels the service exposes, the workflows behind them, and the connectors, plugins and models those workflows use. It is the boundary along which a service ships from one Orion instance to another.

```orion-diagram
{
  "direction": "LR",
  "groups": [
    { "id": "dev", "label": "DEV instance" },
    { "id": "prod", "label": "PROD instance" }
  ],
  "nodes": [
    { "id": "TAGGED", "label": "Tagged entities", "sublabel": "channels · workflows · connectors\ntags: [\"pkg:payments\"]", "type": "accent", "group": "dev" },
    { "id": "ARTIFACT", "label": "payments-1.4.0.json", "sublabel": "one versioned artifact", "type": "infra" },
    { "id": "APPLIED", "label": "Applied package", "sublabel": "staged → activated → receipt", "type": "accent", "group": "prod" }
  ],
  "edges": [
    { "from": "TAGGED", "to": "ARTIFACT", "label": "export" },
    { "from": "ARTIFACT", "to": "APPLIED", "label": "lint → plan → apply" }
  ]
}
```

## The module boundary of a modular monolith

One Orion instance runs many packages side by side. Each deploys, promotes and rolls back on its own schedule. All of them share one runtime, one database and one operational surface: one thing to monitor, back up and patch.

That is the trade a package makes explicit. You get the independence of separate services at the level that matters for change management. You do not pay for a separate deployment, pipeline and on-call surface per service.

## Membership is a label

An entity belongs to a package because it carries the label:

```json
{ "channel_id": "payments", "tags": ["pkg:payments"], "...": "..." }
```

There is no package registry to keep in step and no directory layout to obey. Tagging is the whole membership rule. A service can be re-cut, split in two or absorbed into another, by changing labels.

## Closure: what travels with a channel

Export selects channels, by tag or by id, and works outward from there. The *closure* is what it collects:

- each selected channel, and the workflow that channel names;
- every connector those workflows reference;
- every [plugin](./plugins.md) whose functions they call, at the exact version and component digest serving them on the source, with the component inlined when the export is asked to carry artifacts;
- every [model](./models.md) they name by literal `model_infer` id, carried as its manifest and its artifact *reference* rather than its bytes.

A model's bytes stay in the bucket. The target fetches the object through a storage connector of the same name when it admits the model. That connector is therefore a stated requirement (`requires.storage`) unless the package carries it.

The channel is the unit of selection because each channel names exactly one workflow. Selecting the endpoints selects the service.

What the closure deliberately does not pull in is anything belonging to someone else. A `channel_call` target you did not select is not swept up. It is recorded as a *requirement*: a name the package uses but does not contain. So is a plugin or a model the source does not serve active, and the storage connector a carried model is fetched through. Requirements keep packages small and their boundaries stated, and the target instance is checked for each one before anything is written.

## Two forms of the same thing

- **Source form**: a directory of entity JSON files, one per channel, workflow and connector, plus a `plugin.toml` per plugin and a model manifest per model, each with its artifact beside it. This is what you author, review and keep in git. The [shipped examples](../get-started/tutorials/examples.md) are packages in this form.
- **Artifact form**: one JSON document carrying the entities plus a name, a version, the Orion version it came from, and a content hash. This is what travels between instances.

Two commands produce an artifact, and the downstream verbs cannot tell them apart. `package export` reads a live instance; [`compile`](../reference/cli/orion-server/compile.md) builds one from a directory with no instance to export from. `compile` is also the step that resolves the authoring conveniences source form may use: `$from` for a shared value, `use` for a task fragment. Artifact form never carries either. The hash, the receipt and the running engine only ever see resolved documents.

The hash is computed over importable content only. Versions, statuses and timestamps are the target's business, not the artifact's, so the same logic hashes the same whichever instance exported it.

## Receipts and immutability

A target instance remembers what was applied to it: one *receipt* per package version. Receipts make two guarantees mechanical rather than procedural:

- **An applied version is content-immutable.** Re-applying the version a target currently runs is a no-op; a changed artifact reusing an applied version is refused. Content changes ride a version bump. [`compile --version content`](../reference/cli/orion-server/compile.md#content-versions) makes that bump automatic by naming the version after the content hash.
- **A package can require an Orion version.** A set's [`package` document](../reference/cli/shared-definitions.md#the-package-document) declares a range such as `>=1.8.2, <2`. Offline commands check the running binary against it, and `plan` and `apply` check the target.
- **Applied means serving.** `apply` records a version as applied only after the reload it caused serves every member of the package. A member the reload quarantined fails the apply and leaves the receipt `staged`.
- **Rollback is a re-apply.** Applying the previous version makes it current again. Entities roll forward carrying the old content, and the receipt history records both moves.
- **A node can apply its own packages.** [`[packages] apply`](../reference/configuration/packages.md) names artifacts a node applies at startup, before it reports ready. A restart is a no-op, and a version a later one superseded is left as it is.
- **A receipt records what its version carried.** That inventory is what [`apply --prune`](../reference/cli/orion-server/package.md#prune-what-a-version-dropped) measures from. It removes what the previous version carried and the new one does not, and never touches what another package carries.

## Next steps

- [Test and promote a service](../get-started/tutorials/test-and-promote.md): export a package and apply it to a second instance, start to finish.
- [Promote between environments](../operate/maintain/promotion.md): the five verbs, the secrets rules, and the `requires` boundary in detail.
- [Run the example packages](../get-started/tutorials/examples.md): the deployable example packages in source form.
- [The entity lifecycle](./lifecycle.md): the draft, active and archived rules that `apply` drives on your behalf.
