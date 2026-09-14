<!-- description: The success and error envelopes every admin endpoint returns, how each list endpoint pages and sorts, and the five errors you will meet most. -->
<!-- type: reference -->
<!-- last_verified: 2026-09-14 -->

# Response envelopes

The shape of an admin success body and an admin error body, and how list endpoints page.

Every admin 2xx body puts its payload under a top-level `data` key — one shape, so one unwrapping function works everywhere:

```json
{ "data": { "workflow_id": "wf_...", "name": "Order Processing", "...": "..." } }
```

List endpoints add pagination counters alongside it: `limit` and `offset` always, `total` where the endpoint computes it. The trace list makes `total` opt-in through `?include_total=true`, and adds `next_cursor`:

```json
{ "data": [ ... ], "total": 137, "limit": 50, "offset": 0 }
```

Pre-1.0 responses differed for ten handlers — the [upgrade guide](../../releases/upgrade-to-1.0/index.md) has the full list.

## Paging and sorting by endpoint

**Rationale.** Traces use keyset paging because that table can grow without
bound. Smaller administrative collections retain offset paging. Clients should follow the endpoint contract below rather than assuming every collection has the same sorting controls.

Not every list takes the same query parameters. The asymmetry is contract, not accident. The trace list pages by keyset because its table is the one that grows without bound. The narrower lists have result sets small enough that sorting client-side is cheaper than supporting it server-side.

| Endpoints | `limit` / `offset` | `sort_by` / `sort_order` | Other |
|---|:---:|:---:|---|
| `/workflows`, `/channels`, `/connectors` and their `/export` | Yes | Yes | `?tag=`, `?status=` filters |
| `/traces` | Yes | Yes | `?cursor=` (keyset), `?include_total=true`; the response adds `next_cursor` and omits `total` unless asked |
| `/audit-logs` | Yes | No | `?start_time=` / `?end_time=` (RFC 3339 or naive), `limit` clamped to 1–1000 |
| `/trace-dlq`, `/packages`, `/{id}/versions` | Yes | No | — |

`limit` and `offset` are therefore the only two you can rely on everywhere.

Errors follow one structure across both planes. See [Errors & Response Envelopes](../errors.md#the-error-envelope).

## Common errors

| Operation | Status/code | Corrective action |
|---|---|---|
| Create or update invalid JSON | `400 VALIDATION_ERROR` | Correct the field paths in `details`, then call `/validate` before writing |
| Read an unknown ID | `404 NOT_FOUND` | Verify the resource kind, ID, and target instance |
| Reuse an ID, channel name, route, or immutable package version | `409 CONFLICT` | Inspect the existing resource; create a new entity version when changing active content |
| Activate with a missing dependency or invalid transition | `400 VALIDATION_ERROR` | Run the same status request with `?dry_run=true` and resolve every reported error |
| Call without a valid admin credential | `401 UNAUTHORIZED` | Supply the configured header and key format |

See [Errors & Response Envelopes](../errors.md) for the complete registry and response shapes. Branch on `error.code`, not the human-readable message.

## Related

- [Admin API](./index.md): every admin resource, and the contracts they share.
- [Errors and response envelopes](../errors.md): every code these endpoints return.
- [OpenAPI specification](../openapi.md): the generated contract, and where to fetch it.
- [Authentication](./authentication.md): the one error that precedes every other.
