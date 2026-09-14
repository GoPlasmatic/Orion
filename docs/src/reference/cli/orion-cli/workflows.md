<!-- description: orion-cli workflows creates, versions, activates, rolls out, tests, exports, imports and diffs workflows, with --dry-run and --defer-reload on transitions. -->
<!-- type: reference -->
<!-- last_verified: 2026-09-14 -->

# `orion-cli workflows`

Manages workflows. Alias: `rules`.

## Synopsis

```bash
orion-cli workflows <subcommand> [args] [flags]
```

## Description

`diff` answers the question `import` would act on. It matches local items to stored ones by `workflow_id`, the key an import collides on. It compares the server's `content_hash` when the file carries one, as an exported artifact does. For a hand-authored file it falls back to the importable fields, the same projection the server hashes, so the two answers cannot disagree. Fields that a re-import never writes (`version`, `status`, `created_at`) are ignored, so a file exported and diffed straight back reports every workflow unchanged.

For promoting a whole service rather than one resource kind, use
`orion-server package diff`, which covers channels and connectors too and
compares the closure as a unit.

## Subcommands

| Subcommand | Description |
|------------|-------------|
| `list` | List workflows; filter with `--status` and `--tag`. Sorts by `priority`, `name`, `status`, `created_at`, `updated_at`. |
| `get <id>` | Show a workflow; `--verbose` includes condition and tasks. |
| `create` | Create a workflow from JSON: `-f <file>`, `-d <json>`, or `--stdin`; `--id` sets the workflow id instead of generating one. |
| `update <id>` | Replace a workflow definition. Only drafts accept updates. |
| `delete <id>` | Delete a workflow; prompts unless `--yes`. |
| `activate <id>` | Activate a draft workflow. `--dry-run` pre-flights; `--defer-reload` batches. |
| `archive <id>` | Archive an active workflow. Same two flags. |
| `dependencies <id>` | Show the workflow's connectors and `channel_call` targets. Alias: `deps`. |
| `validate` | Validate a definition without creating it. Exits `1` when invalid. |
| `rollout <id> -p <n>` | Update the rollout percentage. `--defer-reload` batches. |
| `versions <id>` | List version history; pages with `--limit` / `--offset`. |
| `new-version <id>` | Create a new draft version from the active one. |
| `test <id>` | Dry-run the workflow with sample data; `--metadata <json>`, `--trace`. |
| `export` | Export workflows as JSON; filter with `--status`, `--tag`. |
| `import -f <file>` | Bulk-import from a JSON array file; `--dry-run` previews, `--on-conflict` sets the collision rule. |
| `diff -f <file>` | Compare a local file against server state. Exits `1` when anything differs. |

## Options

`activate` and `archive` take two flags that matter for promotion:

| Flag | Description |
|------|-------------|
| `--dry-run` | Run every gate the real transition would run and report the findings, writing nothing. Exits `1` when the transition would be refused, so it gates a script. |
| `--defer-reload` | Commit the row but leave the running engine serving the previous active set. Batch several changes, then `orion-cli engine reload` once. |

`import` takes `--on-conflict` — what an already-stored id means:

| Value | Behaviour |
|-------|-----------|
| `fail` | Default. The conflicting item is refused and reported. |
| `skip` | The conflicting item is left as it is and counted as skipped. |
| `new_version` | Upsert: the draft is replaced in place, or a new draft version is cut over an active entity. Identical content is a no-op. |

## Examples

```bash
orion-cli workflows test order-enrichment -f payload.json --trace
```

## Related

- [Workflows](../../../concepts/workflows.md): what a workflow is.
- [Version and roll out changes](../../../guides/author/versioning.md): draft, activate, roll out and roll back in practice.
- [Admin API › Workflows](../../admin-api/workflows.md): the endpoints these subcommands call.
- [`orion-server package`](../orion-server/package.md): promoting a whole service rather than one workflow.
- [`orion-cli` commands](./index.md): every `orion-cli` subcommand.
