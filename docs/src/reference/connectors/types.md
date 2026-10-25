<!-- description: The seven connector types Orion has — http, kafka, db, cache, es, smtp and storage — with what each backs and the task functions it serves. -->
<!-- type: hub -->
<!-- last_verified: 2026-09-14 -->

# The seven types

The seven types a connector may be, and the `config` each one takes.

| Type | Backs | Task functions |
|------|-------|----------------|
| [`http`](./http.md) | REST APIs and webhooks | `http_call` |
| [`db`](./db.md) | PostgreSQL, MySQL, SQLite | `data_query`, `data_write`, `db_read`, `db_write` |
| [`cache`](./cache.md) | Per-node in-process memory | `cache_read`, `cache_write` |
| [`storage`](./storage.md) | S3-compatible object storage (presign + metadata only) | `storage_presign`, `storage_head` |

Type values match case-insensitively. Any other value is refused with the valid list. A stored connector whose config no longer parses fails to load and surfaces as a connector load issue.

**Required** `yes` on a type's field table means create and update are refused without the field. `—` in **Default** means the field is optional and unset until you set it.

## Related

- [Connector types](./index.md): every type, and the shared blocks all of them carry.
- [Task functions](../functions/index.md): the functions that call through a connector.
- [Shared connector blocks](./common.md): what every type carries whatever it is.
- [Portable data dialect](../data-dialect.md): the language `db` serves.
