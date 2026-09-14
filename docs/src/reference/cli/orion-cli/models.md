<!-- description: orion-cli models registers, admits, activates, versions and exports ONNX models by artifact reference, with --wait polling for the admission verdict. -->
<!-- type: reference -->
<!-- last_verified: 2026-09-14 -->

# `orion-cli models`

Manages [models](../../../concepts/models.md) — ONNX artifacts held in object
storage and admitted before they serve. Alias: `model`. Every subcommand
answers `400` against a node with `models.enabled = false`.

## Synopsis

```bash
orion-cli models <subcommand> [args] [flags]
```

## Description

The artifact bytes never pass through the CLI. They sit in an S3-compatible
bucket behind a [`storage` connector](../../connectors/storage.md), and a registration
names them by connector, object key and `sha256:` digest. `create` answers at once with `admission.state = "pending"` while a node fetches, verifies, parses
and probes the object.

`--wait` (on `create` and `admit`) polls until the verdict lands: exit `1` when
admission fails, `2` on timeout. `--interval` sets the poll period (default
`2` seconds) and `--timeout` the deadline (default `900`).

## Subcommands

| Subcommand | Description |
|------------|-------------|
| `list` | List models; filter with `--status`, `--tag`, `--admission` (`pending`, `passed`, `failed`). Sorts by `model_id`, `status`, `created_at`, `updated_at`. |
| `get <id>` | Show a model with its admission verdict and this node's residency; `--verbose` includes the manifest. |
| `create` | Register a draft: `-f <manifest.json>` plus `--connector`, `--key` and `--digest` naming the artifact. `--signature <path>` supplies the Ed25519 signature over the digest that `[models.trust]` requires; `--tag` is repeatable; `--wait` polls until the verdict lands. |
| `update <id>` | Replace the draft's manifest, artifact reference, signature or tags. |
| `delete <id>` | Delete every version; prompts unless `--yes`. |
| `activate <id>` | Activate a draft. `--dry-run` pre-flights; `--defer-reload` batches. Refused with `409` until the admission verdict is `passed`. |
| `archive <id>` | Archive an active model. Same two flags. Refused while an active workflow names it by literal id. |
| `admit <id>` | Run admission again — fetch, verify and probe the artifact once more. `--wait` polls for the verdict. |
| `dependencies <id>` | The active workflows naming this model. Alias: `deps`. |
| `validate` | Validate a manifest and artifact reference without registering them. Exits `1` when invalid. |
| `versions <id>` | List version history; pages with `--limit` / `--offset`. |
| `new-version <id>` | Create a new draft version from the active one. |
| `export` | Export models as JSON; filter with `--status`, `--tag`. References only — the bytes never travel. |
| `import -f <file>` | Bulk-import from a JSON array file; each item is queued for admission on the target. `--dry-run` previews, `--on-conflict` sets the collision rule. |

## Examples

```bash
orion-cli models create -f model.json --connector models --key fraud/v3.onnx --digest sha256:9f1c... --wait
```

## Related

- [Models](../../../concepts/models.md): what a model is and how admission works.
- [Serve a model](../../../guides/extend/models.md): registering, admitting and calling one.
- [Admin API › Models](../../admin-api/models.md): the endpoints behind the subcommands.
- [`orion-cli` commands](./index.md): every `orion-cli` subcommand.
