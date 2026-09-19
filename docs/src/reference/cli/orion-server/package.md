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
| `lint` | Validate an artifact offline: entity shapes, closure completeness, content hash, a `content-<12 hex>` version against that hash, and the cross-reference checks `lint <dir>` runs. Exits non-zero on **errors**; warnings and inventory notes print without failing. |
| `plan` | Pre-flight an artifact against a target with zero writes. |
| `apply` | Stage all entities, activate in dependency order, reload once, check the reloaded generation serves them, then record the receipt. Idempotent: re-applying the package's current version is a no-op, while re-applying a version a later one superseded rolls the entities back to it and makes it current again. `plan` names the version that superseded it. |
| `diff` | Report drift between an artifact and a running instance. Exits non-zero when anything differs. |

## Options

| Flag | Used by | Description |
|------|---------|-------------|
| `-s, --server <url>` | `export`, `plan`, `apply`, `diff` | Base URL of the source or target instance. |
| `-f, --file <path>` | `lint`, `plan`, `apply`, `diff` | Path to the artifact file. |
| `--tag <tag>` | `export` | Select every channel carrying this tag. |
| `--channels <ids>` | `export` | Select channels by id, comma-separated or repeated. |
| `--name <name>` | `export` | Package name. |
| `--version <ver>` | `export` | Package version. Applied versions are immutable; any content change needs a bump. `content` derives it from the content hash, as [`compile`](./compile.md#content-versions) does. |
| `--version-prefix <prefix>` | `export` | With `--version content`, `<prefix>-<12 hex>` instead of `content-<12 hex>`. |
| `-o, --output <path>` | `export` | Write the artifact here instead of stdout. |
| `--signatures <dir>` | `plan`, `apply` | Attach the detached signatures in `<dir>` to the artifact's plugins and models before anything is sent. See [Signatures at deploy time](#signatures-at-deploy-time). |
| `--include-artifacts` | `export` | Inline each plugin's component as base64, so the artifact installs the plugin on a target that has never seen it. Without it a plugin travels as manifest and digest, and `plan` fails unless the target already holds that digest. |

## Applied means serving

A reload succeeds even when an entity it loads does not: the entity is quarantined and everything else serves. `apply` therefore reads what the generation it published could not load, and fails when any of that is a member of the package:

```
error: 1 entity of nightly@2.0.0 is quarantined on https://prod.orion.internal:
  connectors/crm: secret_resolution: environment variable 'CRM_TOKEN' is not set
Error: nightly@2.0.0 is not serving on https://prod.orion.internal — the receipt stays staged; fix the cause and re-run apply
```

The receipt is flipped to `applied` only after that check, so a failed apply leaves it `staged` and a re-run after the fix completes it. A member counts when its own row is refused: a plugin, model or connector the package carries, or a channel. A workflow counts when a channel of another package bound to it is refused.

Re-applying the version a target already runs is not blind either. `apply` reads `GET /engine/status` and fails the same way when a member is quarantined, for example after the node restarted with `cron.enabled = false`. That receipt is already `applied` and stays so, so the message says to fix the cause and reload. `plan` reads the same endpoint and warns when the target has cron, plugins or models switched off for something the package needs. It also warns when the target already quarantines one of the package's members. A target older than these fields cannot say, and `apply` prints a warning rather than fail.

## Signatures at deploy time

A signature belongs to whoever holds the key, which is the deployment, not the package. One image can serve several deployments, each trusting its own key, so the signatures cannot live in the definition set. `plan` and `apply` read them from a directory with `--signatures <dir>` instead.

Each file is the base64 Ed25519 signature over one artifact's digest, as [`orion-server plugin sign -o <dir>`](./plugin.md) writes it. A plugin's file is `<plugin id>.sig` or `<component file>.sig`, and a model's is `<model id>.sig` or the file name of its bucket key. The id form wins when both exist. Only `.sig` files directly in the directory count, so a mounted Kubernetes secret reads by the names it shows.

The signatures are attached in memory. The artifact file, its version and its `content_hash` do not move, because a signature is not content. Before anything is sent, each plugin and model is reported on one line:

```
signed    acme.scoring   <- /run/plugin-signatures/scoring.wasm.sig
carried   acme.legacy    (signature from the artifact; none in /run/plugin-signatures)
unsigned  acme.pairing   (no acme.pairing.sig or pairing.wasm.sig)
```

A file in the directory takes precedence over a signature the artifact already carries: an export's signature was made for the source's keys. A file that matches nothing is an error listing the names that would have matched. A misnamed file must not leave a plugin silently unsigned. So is a file two entries claim, and one that is not a signature. An unsigned plugin is only reported, and a target with trust keys refuses it naming the plugin.

A re-apply of the version the target already runs still compares signatures. When one differs from what the target stores, as after a key rotation, `apply` imports and activates only those plugins and models. It then reloads and prints `re-signed plugins '<id>'`. The receipt does not move, because the content did not.

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
