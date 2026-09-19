<!-- description: orion-server package exports, lints, plans, applies and diffs a promotion artifact of channels, workflows, connectors, plugins and models between instances. -->
<!-- type: reference -->
<!-- last_verified: 2026-09-19 -->

# `orion-server package`

Exports a package (selected channels, their workflows, and every connector those workflows reference) and promotes it between instances. The artifact is one JSON document. The model is described in [Promote between environments](../../../operate/maintain/promotion.md).

## Synopsis

```bash
orion-server package <export|lint|plan|apply|diff> [flags]
```

## Description

Every subcommand except `lint` calls an instance's admin API, authenticating with the `ORION_ADMIN_TOKEN` environment variable. A warning is printed when the token would travel over plain `http://` to any host but the local machine.



**Plugins are the fourth member.** `export` resolves every plugin function a selected workflow calls to the active version and digest serving it on the source; `GET /workflows/{id}/dependencies` reports them under `plugins`. It carries each under `plugins[]`: manifest, digest, tags, and the component when `--include-artifacts` is set. A plugin the source no longer serves at that digest is recorded under `requires.plugins` instead, and `plan` checks the target has it active. `apply` stages plugins before connectors and activates them before workflows, so a promoted channel never quarantines for a function that arrived in the same package. `plugins` is omitted from the document and the content hash when a package carries none, so every receipt applied before plugins existed stays valid.

**Models are the fifth.** `export` collects every model a selected workflow names by literal `model_infer` id and carries each active one under `models[]` as `GET /models/export` gives it. That is the manifest and the artifact **reference** (`connector`, `key`, `digest`), never the bytes. The target fetches the object through its own storage connector of that name at admission. Two requirements follow. Each such connector the package does not carry is recorded under `requires.storage`. Both `plan` and `apply` check the target has a `storage` connector of that name before anything is written. A model import fails at write without it. A model the source does not serve active is recorded under `requires.models` as `{id, version, digest}`. The version and digest are what the source holds, or `0` and empty when it holds none. `plan` checks the target serves it active, at that digest when one is named. `apply` stages models after connectors and before workflows, then **waits for the target to admit each one**. Admission is the fetch, the digest check, the parse and the probe, polled on `GET /models/{id}` with progress on stderr and a 900 s ceiling. Only then is the model activated, ahead of the workflows that name it. The reload at the end therefore never quarantines a channel for a model that arrived in the same package. A model the target refuses at admission stops the apply naming the stage and reason. `models` is omitted from the document and the hash when a package carries none, like `plugins`. [`compile`](../orion-server/compile.md) writes the same member from a manifest's `reference` and the file beside it.

## Subcommands

| Subcommand | Description |
|------------|-------------|
| `export` | Compute the dependency closure from a running instance and write the artifact. |
| `lint` | Validate an artifact offline: entity shapes, closure completeness, content hash, and the cross-reference checks `lint <dir>` runs. Exits non-zero on **errors**; warnings and inventory notes print without failing. |
| `plan` | Pre-flight an artifact against a target with zero writes. |
| `apply` | Stage all entities, activate in dependency order, reload once, record the receipt. Idempotent: re-applying the package's current version is a no-op, while re-applying a version a later one superseded rolls the entities back to it and makes it current again. `plan` names the version that superseded it. |
| `diff` | Report drift between an artifact and a running instance. Exits non-zero when anything differs. |

## Options

| Flag | Used by | Description |
|------|---------|-------------|
| `-s, --server <url>` | `export`, `plan`, `apply`, `diff` | Base URL of the source or target instance. |
| `-f, --file <path>` | `lint`, `plan`, `apply`, `diff` | Path to the artifact file. |
| `--tag <tag>` | `export` | Select every channel carrying this tag. |
| `--channels <ids>` | `export` | Select channels by id, comma-separated or repeated. |
| `--name <name>` | `export` | Package name. |
| `--version <ver>` | `export` | Package version. Applied versions are immutable; any content change needs a bump. |
| `-o, --output <path>` | `export` | Write the artifact here instead of stdout. |
| `--include-artifacts` | `export` | Inline each plugin's component as base64, so the artifact installs the plugin on a target that has never seen it. Without it a plugin travels as manifest and digest, and `plan` fails unless the target already holds that digest. |

## Examples

```bash
orion-server package export -s https://dev.orion.internal --tag payments --name payments --version 1.4.0 --include-artifacts -o pkg.json
```

## Related

- [Promote between environments](../../../operate/maintain/promotion.md): the promotion model and its guarantees.
- [Packages](../../../concepts/packages.md): the versioned unit these verbs move.
- [`orion-server compile`](./compile.md): building the same artifact from a directory.
- [`orion-cli packages`](../orion-cli/packages.md): the receipts `plan` and `apply` write.
- [`orion-server` commands](./index.md): every `orion-server` subcommand.
