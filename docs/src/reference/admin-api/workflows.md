<!-- description: The workflow endpoints: create, test, activate, roll out, version, import and export, plus the dependency read and the offline dry run. -->
<!-- type: reference -->
<!-- last_verified: 2026-09-14 -->

# Workflow endpoints

The endpoints that create, test, version and roll out a workflow.

| Method | Path | Description |
|--------|------|-------------|
| POST | `/api/v1/admin/workflows` | Create workflow as a draft. Optional `workflow_id` supplies a custom ID; `tags: ["..."]` supplies selection labels used by `?tag=` filters and package export |
| GET | `/api/v1/admin/workflows` | List workflows. Filter with `?tag=`, `?status=` |
| GET | `/api/v1/admin/workflows/{id}` | Get workflow by ID |
| PUT | `/api/v1/admin/workflows/{id}` | Update draft workflow |
| DELETE | `/api/v1/admin/workflows/{id}` | Delete workflow (all versions) |
| PATCH | `/api/v1/admin/workflows/{id}/status` | Change status (active/archived). Activation refuses missing/mistyped connector references. `?dry_run=true` / `?reload=defer` — see [Status changes](./status-changes.md) |
| GET | `/api/v1/admin/workflows/{id}/versions` | List workflow version history |
| POST | `/api/v1/admin/workflows/{id}/versions` | Create new draft version from active workflow |
| PATCH | `/api/v1/admin/workflows/{id}/rollout` | Update rollout percentage. `?reload=defer` commits without rebuilding the engine |
| POST | `/api/v1/admin/workflows/{id}/test` | Dry-run on sample payload |
| GET | `/api/v1/admin/workflows/{id}/dependencies` | What the tasks reference: connector names (with the referencing function) and static `channel_call` targets, plus a flag when targets resolve dynamically. For closure tooling |
| POST | `/api/v1/admin/workflows/import` | Bulk import workflows (as drafts). `?dry_run=true` validates without writing; `?on_conflict=fail\|skip\|new_version` picks what an existing id means |
| GET | `/api/v1/admin/workflows/export` | Export workflows. Filter with `?tag=`, `?status=` |
| POST | `/api/v1/admin/workflows/validate` | Validate workflow definition |

## Related

- [Admin API](./index.md): every admin resource, and the contracts they share.
- [Workflows](../../concepts/workflows.md): what a workflow is.
- [Workflow reference](../workflows.md): the document these endpoints accept.
- [Status changes](./status-changes.md): activating and archiving one.
