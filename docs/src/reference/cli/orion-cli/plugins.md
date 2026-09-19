<!-- description: orion-cli plugins uploads, versions, activates, validates and exports WebAssembly plugins, with the signature and component flags an upload takes. -->
<!-- type: reference -->
<!-- last_verified: 2026-09-19 -->

# `orion-cli plugins`

Manages [plugins](../../../concepts/plugins.md) — WebAssembly components that add
task functions. Alias: `plugin`. Every subcommand answers `400` against a node
with `plugins.enabled = false`.

## Synopsis

```bash
orion-cli plugins <subcommand> [args] [flags]
```

## Description

A component is uploaded, never referenced: the server hashes, compiles and
probes it before the draft exists. Without `--include-artifacts` an export
carries manifests and digests only, and imports only into a target that
already holds those digests.

## Subcommands

| Subcommand | Description |
|------------|-------------|
| `list` | List plugins; filter with `--status`, `--tag`. Sorts by `plugin_id`, `status`, `created_at`, `updated_at`. |
| `get <id>` | Show a plugin with this node's load state; `--verbose` includes the manifest. |
| `create` | Upload a draft from `-f <plugin.toml>`. `--component <path>` overrides the manifest's `component`; `--signature <path>` supplies the base64 Ed25519 signature a server with `[plugins.trust]` keys requires, as [`orion-server plugin sign`](../orion-server/plugin.md) writes it; `--tag` is repeatable. |
| `update <id>` | Replace the draft's manifest and component. Same flags; `--tag` replaces the stored tags when given. |
| `delete <id>` | Delete every version and any component nothing names; prompts unless `--yes`. |
| `activate <id>` | Activate a draft. `--dry-run` pre-flights; `--defer-reload` batches. Refused when an active dependant no longer satisfies the new version's schema. |
| `archive <id>` | Archive an active plugin. Same two flags. Refused while an active workflow calls one of its functions. |
| `dependencies <id>` | The functions this plugin declares and the active workflows calling them. Alias: `deps`. |
| `validate` | Validate a manifest and component without uploading. Exits `1` when invalid. |
| `versions <id>` | List version history; pages with `--limit` / `--offset`. |
| `new-version <id>` | Create a new draft version from the latest. |
| `export` | Export plugins as JSON; filter with `--status`, `--tag`. `--include-artifacts` inlines each component as base64 so the file imports anywhere. |
| `import -f <file>` | Bulk-import from a JSON array file; `--dry-run` previews, `--on-conflict` sets the collision rule. |

## Examples

```bash
orion-cli plugins create -f plugin.toml --tag codecs
```

## Related

- [Plugins](../../../concepts/plugins.md): what a plugin is.
- [Write a plugin](../../../guides/extend/plugins.md): the build, the upload and the first call.
- [Plugin manifest and ABI](../../plugin-manifest.md): the manifest `create` reads and the trust signature.
- [Admin API › Plugins](../../admin-api/plugins.md): the endpoints behind the subcommands.
- [`orion-cli` commands](./index.md): every `orion-cli` subcommand.
