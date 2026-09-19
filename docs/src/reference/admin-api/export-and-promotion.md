<!-- description: Moving entities between instances over the API: the export and import endpoints, the on_conflict modes, change grouping, and secrets in a bundle. -->
<!-- type: reference -->
<!-- last_verified: 2026-09-19 -->

# Export and promotion

The endpoints that carry entities between instances, and the three decisions an import makes.

All five entity kinds export and import, so an estate can live in git rather than only in the database. Each `/export` emits the shape its `/import` accepts, so the round trip needs no reshaping in between.

Every `/import` endpoint accepts at most **1000 items per request** and answers `400 VALIDATION_ERROR` above that — split a larger estate into batches. The request is also bounded by `server.max_admin_body_size`, which a batch of large workflows can reach well before the item cap does.

| Method | Path | Description |
|--------|------|-------------|
| GET | `/api/v1/admin/{workflows,channels,connectors,plugins,models}/export` | Export every entity of that kind. `?tag=` and `?status=` narrow the set. Plugin components are inlined only with `?include_artifacts=true`; a model always exports as a reference, never as bytes |
| POST | `/api/v1/admin/{workflows,channels,connectors,plugins,models}/import` | Bulk import. `?on_conflict=` selects the collision policy; `?dry_run=true` reports what would happen |

Each export reads inside **one repeatable-read transaction**, so the result is a consistent snapshot. Rows mutated mid-export cannot be skipped or duplicated. On MySQL this relies on InnoDB's default REPEATABLE READ isolation, and lowering it session-wide weakens the guarantee.

Every entity response also carries `content_hash`: `sha256:…` over the canonical
*importable content*, with the DB-owned fields (`version`, `status`, timestamps,
`rollout_percentage`) excluded. Equal hashes mean "importing one over the other is a no-op", which is how drift is detected without comparing bodies. Hashes are computed over stored values, so only `env://`/`vault://`-authored entities hash identically to their masked exports.

The operator's guide to using these — the `orion-server package` verbs, the receipt model, secrets handling, and mid-apply failure modes — is [Promote Between Environments](../../operate/maintain/promotion.md).

## Promoting over an existing estate (`on_conflict`)

By default an import is create-only: an item whose `workflow_id` / `channel_id` / connector `name` is already stored becomes one `errors[]` entry. `?on_conflict=` selects what "already stored" means instead:

| Mode | Existing draft | Existing active | Identical content | Connectors (unversioned) |
|---|---|---|---|---|
| `fail` (default) | refused | refused | refused | refused |
| `skip` | `skipped` | `skipped` | `skipped` | `skipped` |
| `new_version` | draft replaced (`updated_draft`) | new draft version cut with the item's content (`new_version`) | nothing written (`unchanged`) | updated in place (`updated`) |

Content comparison excludes the DB-owned fields: `version`, `status`, timestamps and `rollout_percentage`. Re-importing an unmodified export therefore reports `unchanged` for everything. **Re-running the same artifact is a no-op**, which is what makes the import safe to retry from CI. An *archived* entity with identical content still gets a new draft version: the point of re-importing it is to activate it again. A plugin or model `signature` is not content, but an item carrying a *different* signature is not `unchanged`. It is written, as `updated_draft` or `new_version`, so a signature attached at deploy time reaches the row. An item carrying none keeps the stored one. The response's `results` array carries one `{index, id, action}` per non-failed item; `?dry_run=true` composes with every mode and reports the action the real import would take.

The two upsert-ish modes refuse an id that appears twice in one batch — the second item would silently rewrite what the first had staged.

## Grouping a multi-request operation (`X-Orion-Change-Context`)

A promotion is many API calls. Send the same `X-Orion-Change-Context` header on each, for example `package=payments@1.4.0`. Every audit row the operation produces then carries it under `details.change_context`, so the trail can be filtered back into the operation that caused it. Free-form, truncated at 256 bytes. Imports additionally write one audit row per entity written, alongside the batch summary row.

## Secrets in an exported bundle

A connector export is masked, which is what makes it safe to commit. Only a connector authored with an `env://` or `vault://` reference round-trips. A literal credential exports as `"******"` and is **refused** on import, rather than stored as a credential that fails at the first request. The rules are in [Connector Types › Secret masking](../connectors/masking.md), and what they mean for promotion is in [Promote Between Environments](../../operate/maintain/promotion.md#secrets-survive-the-trip-if-authored-as-references).

`POST /{kind}/validate` runs the same validator `POST /{kind}` runs, so `valid: true` means create would accept the payload — it is never laxer. An `env://` reference unset on the validating host is a **warning**, not an error. A CI runner holding no production secrets can still check a bundle.

## Related

- [Admin API](./index.md): every admin resource, and the contracts they share.
- [Promote between environments](../../operate/maintain/promotion.md): the operator's guide to these endpoints.
- [Packages](./packages.md): the receipts an apply writes.
- [Secret masking](../connectors/masking.md): why a literal secret does not survive a bundle.
