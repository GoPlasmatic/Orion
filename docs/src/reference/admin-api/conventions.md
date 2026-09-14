<!-- description: The contracts every admin endpoint shares: the API key, the response envelopes, the lifecycle, status changes, and export and promotion. -->
<!-- type: hub -->
<!-- last_verified: 2026-09-14 -->

# Admin API conventions

What holds for every admin endpoint whatever resource it addresses.

| Page | Holds |
|---|---|
| [Authentication](./authentication.md) | the one header the server reads, the key forms, rotation, and the failure backoff. |
| [Response envelopes](./envelopes.md) | the success and error bodies, paging and sorting per endpoint, and the common errors. |
| [Lifecycle over the API](./lifecycle.md) | what a write does to status and version, and which writes reach the engine. |
| [Status changes](./status-changes.md) | activating and archiving, the `dry_run` pre-flight, and `reload=defer`. |
| [Export and promotion](./export-and-promotion.md) | export and import, `on_conflict`, change grouping, and secrets in a bundle. |

## Related

- [Admin API](./index.md): every admin resource, and the contracts they share.
- [OpenAPI specification](../openapi.md): the generated contract, and where to fetch it.
- [Errors and response envelopes](../errors.md): every code these endpoints return.
- [Entity endpoints](./entities.md): the resources these contracts apply to.
