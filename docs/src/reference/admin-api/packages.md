<!-- description: The package receipt endpoints: listing and reading what an instance has applied, and the content-immutability rule an applied version carries. -->
<!-- type: reference -->
<!-- last_verified: 2026-09-14 -->

# Package endpoints

The receipts an instance keeps of every package applied to it, and the one rule that makes them trustworthy.

A **package** is the channels, workflows, connectors, plugins and models of one service, promoted between instances as a versioned unit ([Promote Between Environments](../../operate/maintain/promotion.md)). This is the single package-aware surface of the admin API.

Packaging itself lives in client tooling built on the per-kind endpoints above: computing an artifact's dependency closure, planning, staging, activating. What the server keeps is one **receipt** per package version. It has to, because the promotion rule cannot be enforced unless the target remembers what was applied:

> **An applied package version is immutable.** The same version arriving with
> a different content hash is refused with a `409`; only a `staged` receipt
> may change; any content change rides a package version bump.

| Method | Path | Description |
|--------|------|-------------|
| GET | `/api/v1/admin/packages` | List receipt rows (paginated, `?limit=`/`?offset=`), ordered by package name, newest first within a package. `?current=true` lists each package's current receipt instead, with its `inventory` |
| GET | `/api/v1/admin/packages/{name}` | One package's receipts, plus `current` — the newest `applied` version |
| PUT | `/api/v1/admin/packages/{name}` | Record or advance a receipt. Body: `{"version", "content_hash", "state": "staged"\|"applied", "inventory"}` |

The intended apply sequence has four steps. **Claim** the receipt as `staged`, which is the atomic same-version-different-content rejection and doubles as a guard against two concurrent applies. Stage the artifact's entities through the `/import` endpoints. Activate them in dependency order: plugins → connectors → models → workflows → channels. Then flip the receipt to `applied`. A failed apply leaves the receipt `staged`, so a corrected re-run at the same version is legal — only a draft can be updated. Re-putting an *older* applied version with its own original hash is also legal, and makes it current again. That is the rollback path: entities roll forward carrying the old content, and nothing moves backward.

A receipt may record an `inventory`: what its version carried, as `{"plugins", "connectors", "models", "workflows", "channels"}`. Each is a list of ids, or connector names, of at most 1000 entries. A staged receipt's inventory is replaced by a PUT that carries one. An applied receipt's is never replaced, but a receipt applied without one gains it on its next re-apply. `package apply --prune` reads the current receipt's inventory to find what a new version dropped. The plain listing leaves inventories out; the package detail and `?current=true` carry them.

Receipts never touch the engine — no reload, no cluster epoch bump. `state`, `content_hash` and `principal` are recorded verbatim; the hash is opaque to the server and compared only for equality.

## Related

- [Admin API](./index.md): every admin resource, and the contracts they share.
- [Packages](../../concepts/packages.md): what a package is.
- [Export and promotion](./export-and-promotion.md): the endpoints that write these receipts.
- [Promote between environments](../../operate/maintain/promotion.md): the operator's guide.
