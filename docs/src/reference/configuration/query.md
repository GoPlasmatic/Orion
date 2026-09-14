<!-- description: The [query] and [write] settings: default and maximum page size, the skip cap, rows per bulk write, and the unfiltered-mutation opt-in. -->
<!-- type: reference -->
<!-- last_verified: 2026-09-14 -->

# Query and write bounds

Safety bounds for the portable `data_query` / `data_write` handlers. Requests over a bound are rejected, never silently clamped or truncated.

## Synopsis

```toml
[query]
default_limit = 100
max_limit = 1000
max_skip = 10000

[write]
max_rows = 1000
allow_unfiltered = false
```

## Options

| Setting | Default | Env var | When to change |
|---|---|---|---|
| `query.default_limit` | `100` | `ORION_QUERY__DEFAULT_LIMIT` | Page size applied when a query omits `limit`. |
| `query.max_limit` | `1000` | `ORION_QUERY__MAX_LIMIT` | Hard cap on page size. Must be ≥ `default_limit`. |
| `query.max_skip` | `10000` | `ORION_QUERY__MAX_SKIP` | Hard cap on the `skip` offset, enforced on every backend. A query skipping more is rejected, never clamped. |
| `write.max_rows` | `1000` | `ORION_WRITE__MAX_ROWS` | Hard cap on rows per bulk insert or upsert. |
| `write.allow_unfiltered` | `false` | `ORION_WRITE__ALLOW_UNFILTERED` | Leave `false` unless a workflow genuinely needs unfiltered `update`/`delete` — which still also requires `"all": true` on the call itself. |

**`include` pages too.** `query.default_limit` and `query.max_limit` govern an `include`'s nested page as well, applied **per parent**. An `include` with no `limit` fetches `default_limit` children *for each parent row*, and one above `max_limit` is rejected. A page of 100 parents with an unbounded-looking `include` is therefore bounded at `100 × default_limit` child rows. Size the two together.

## Related

- [Portable data dialect](../data-dialect.md): the paging and write envelopes these bound.
- [`data_query`](../functions/data_query.md): the function that reads the page bounds.
- [`data_write`](../functions/data_write.md): the function that reads the write bounds.
- [Server configuration](./index.md): every section, by what you are configuring.
