<!-- description: orion-server compile resolves $from and use in a definition set and writes a package artifact, a directory of request bodies, or bulk-import files. -->
<!-- type: reference -->
<!-- last_verified: 2026-09-19 -->

# `orion-server compile`

Compiles a definition set into files the admin API accepts, resolving its authoring conveniences: `$from`, `use` and `$sql`. Needs no config, database, or server.

## Synopsis

```bash
orion-server compile <dir> [-o <PATH>] [--format artifact|dir|bulk]
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

Entities must carry explicit ids for `artifact` only. `apply` activates a channel by `channel_id` and reads activation intent off it. An id-less entity in an artifact is one `apply` would stage and never activate. `dir` and `bulk` emit request bodies, where the server derives an id from the name exactly as it does for a hand-written POST. Leaving `channel_id` out of a definition is an ordinary way to author a set. Committing a server-generated UUID would tie the set to one instance.

## Options

| Flag | Description |
|------|-------------|
| `-o, --output` | A file for `--format artifact` (default: stdout); a directory for `dir` and `bulk`, where it is required. |
| `--format` | `artifact` (default), `dir`, or `bulk`; each is described under [Output formats](#output-formats). |
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

## Related

- [Shared definitions](../shared-definitions.md): the conveniences this command resolves.
- [Packages](../../../concepts/packages.md): what a promotion artifact is.
- [CI/CD with packages](../../../guides/patterns/ci-cd.md): compile in a pipeline, from a repository to an instance.
- [`orion-server package`](./package.md): the verbs that consume the artifact.
- [`orion-server` commands](./index.md): every `orion-server` subcommand.
