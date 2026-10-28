<!-- description: The success and error envelopes every admin endpoint returns, how each list endpoint pages, and the four errors you will meet most. -->
<!-- type: reference -->
<!-- last_verified: 2026-09-14 -->

# Response envelopes

The shape of an admin success body and an admin error body, and how list endpoints page.

Every admin 2xx body puts its payload under a top-level `data` key — one shape, so one unwrapping function works everywhere:

```json
{ "data": { "version": "…", "generation": 3, "...": "..." } }
```

A list answers the same envelope everywhere: `data` plus how to get the next page.

```json
{ "data": [ ... ], "limit": 50, "next_cursor": "1753900000123456.0190…" }
```

| Field | Meaning |
|---|---|
| `data` | The rows of this page. |
| `limit` | The page size applied, after clamping to 1–1000 (default 50). |
| `next_cursor` | Present when more rows follow. Pass it back as `?cursor=`, unmodified. Opaque: do not parse it. Absent on the last page, and a page is never empty. |
| `total` | Present only with `?include_total=true`: every row matching the filter, ignoring the page. Absent means "not counted", never "no rows". |

There is no `offset`. Pre-1.0 responses differed for ten handlers — the [upgrade guide](../../releases/upgrade-to-1.0/index.md) has the full list.

## Paging by endpoint

Every list pages by cursor. The lists are ledgers read newest-first. A cursor resumes at an exact position in constant time. An offset would skip or repeat rows as new ones arrive, and cost a scan of everything it skips. The cursor encodes the pair `(created_at, id)`, so a page boundary inside a shared timestamp neither skips nor repeats rows. `total` is opt-in because the count is a full scan of the filtered set on PostgreSQL and InnoDB.

| Endpoints | Order | Parameters |
|---|---|---|
| `/traces`, `/audit-logs`, `/trace-dlq`, `/cron/occurrences` | Newest first | `?limit=`, `?cursor=`, `?include_total=true`, plus the endpoint's filters |
| `/connections`, `/connections/{id}/revisions`, `/connector-packages` | By key (connection then revision; package digest) | `?limit=`, `?cursor=`, `?include_total=true` |
| `/deployments` | Newest revision first | `?limit=`, `?cursor=`; the page also carries `head_revision`, `epoch` and `applied_revision` |

A `limit` of `0` is clamped to 1 on the ledgers and refused with `400` on the catalogues. A cursor an endpoint did not issue is a `400` everywhere. The remaining collections (`/backups`, `/cron/status`, `/functions`) are small, complete catalogues and answer `{ "data": [ ... ] }` with no cursor.

Errors follow one structure across both planes. See [Errors & Response Envelopes](../errors.md#the-error-envelope).

## Common errors

| Operation | Status/code | Corrective action |
|---|---|---|
| Send an invalid definition or body | `400 VALIDATION_ERROR` | Correct the field paths in `details` and send it again |
| Read an unknown ID | `404 NOT_FOUND` | Verify the resource kind, ID, and target instance |
| Apply against a moved head, or reuse an immutable connection revision with new content | `409 CONFLICT` | Read the current head or revision, then plan and apply again |
| Call without a valid admin credential | `401 UNAUTHORIZED` | Supply the configured header and key format |

See [Errors & Response Envelopes](../errors.md) for the complete registry and response shapes. Branch on `error.code`, not the human-readable message.

## Related

- [Admin API](./index.md): every admin resource, and the contracts they share.
- [Errors and response envelopes](../errors.md): every code these endpoints return.
- [OpenAPI specification](../openapi.md): the generated contract, and where to fetch it.
- [Authentication](./authentication.md): the one error that precedes every other.
