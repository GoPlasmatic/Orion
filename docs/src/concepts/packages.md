<!-- description: A package is one Orion service versioned as a unit — its channels, workflows, connectors and plugins — and the boundary along which a service ships. -->
# Packages

A **package** is one service, named and versioned as a unit: the channels it
exposes, the workflows behind them, and the connectors those workflows use. It
is the boundary along which a service ships from one Orion instance to another.

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

One Orion instance runs many packages side by side. Each deploys, promotes and
rolls back on its own schedule, while all of them share one runtime, one
database, and one operational surface — one thing to monitor, back up and patch.

That is the trade a package makes explicit. You get the independence of separate
services at the level that matters for change management, without paying for a
separate deployment, pipeline, and on-call surface per service.

## Membership is a label

An entity belongs to a package because it carries the label:

```json
{ "channel_id": "payments", "tags": ["pkg:payments"], "...": "..." }
```

There is no package registry to keep in step and no directory layout to obey.
Tagging is the whole membership rule, which means a service can be re-cut — split
in two, or absorbed into another — by changing labels.

## Closure: what travels with a channel

Export selects **channels**, by tag or by id, and works outward from there. The
**closure** is what it collects: each selected channel, the workflow that
channel names, every connector those workflows reference, and every
[plugin](./plugins.md) whose functions they call — at the exact version and
component digest serving them on the source, with the component itself
inlined when the export is asked to carry artifacts.

The channel is the unit of selection because each channel names exactly one
workflow. Selecting the endpoints selects the service.

What the closure deliberately does *not* pull in is anything belonging to
someone else. A `channel_call` target you did not select is not swept up; it is
recorded as a **requirement**: a name the package uses but does not contain.
Requirements keep packages small and their boundaries stated, and the target
instance is checked for each one before anything is written.

## Two forms of the same thing

- **Source form**: a directory of entity JSON files, one per channel, workflow
  and connector. This is what you author, review and keep in git; the
  [shipped examples](../getting-started/examples.md) are packages in this form.
- **Artifact form**: one JSON document carrying the entities plus a name, a
  version, the Orion version it came from, and a content hash. This is what
  travels between instances.

Two commands produce an artifact, and the downstream verbs cannot tell them
apart: `package export` reads a live instance, and
[`compile`](../reference/cli.md#compile) builds one from a directory with no
instance to export from. `compile` is also the step that resolves the
authoring conveniences source form may use — `$from` for a shared value, `use`
for a task fragment. Artifact form never carries either: the hash, the receipt
and the running engine only ever see resolved documents.

The hash is computed over importable content only — versions, statuses and
timestamps are the target's business, not the artifact's, so the same logic
hashes the same whichever instance exported it.

## Receipts and immutability

A target instance remembers what was applied to it: one **receipt** per package
version. Receipts are what make two guarantees mechanical rather than
procedural:

- **An applied version is content-immutable.** Re-applying an identical artifact
  is a no-op; a *changed* artifact reusing an applied version is refused. Content
  changes ride a version bump.
- **Rollback is a re-apply.** Applying the previous version makes it current
  again. Entities roll forward carrying the old content, and the receipt history
  records both moves.

## Next steps

- [Test & Promote a Service](../getting-started/test-and-promote.md): export a
  package and apply it to a second instance, start to finish.
- [Promote Between Environments](../operate/promotion.md): the five verbs, the secrets
  rules, and the `requires` boundary in detail.
- [Run the Examples](../getting-started/examples.md): the deployable example packages in source
  form, ready to deploy.
- [The Entity Lifecycle](./lifecycle.md): the draft/active/archived rules that
  `apply` drives on your behalf.
