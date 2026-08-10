-- Indexes for the trace list path (proposal D8).
--
-- `sort_by=updated_at` has been in the sort whitelist since 0.1 with no index
-- behind it on any backend, so every page of `GET /api/v1/admin/traces` sorted
-- that way was a full scan plus a filesort of the hottest table in the schema.
-- The column is worth keeping — "what changed most recently" is the query an
-- operator runs during an incident — so it gets an index rather than being
-- dropped from the whitelist.
--
-- The default ordering is created_at DESC and keyset pagination breaks ties on
-- id, so the single-column created_at index is replaced by (created_at, id): a
-- strict superset that still serves the retention delete's `created_at <
-- cutoff` and turns the keyset predicate into one index range scan. MySQL
-- needs the table named on DROP INDEX; the index names match the other two
-- backends so `schema_parity` can compare them.
--
-- ## Locking, on MySQL specifically
--
-- Adding and dropping a secondary index on InnoDB is online DDL: the server
-- builds it in place and permits concurrent DML throughout, taking an exclusive
-- metadata lock only briefly at each end. `ALGORITHM=INPLACE LOCK=NONE` is
-- stated rather than left to DEFAULT so a server or storage engine that
-- *cannot* do it online fails the migration loudly instead of silently locking
-- `traces` against writes for the length of the build — which is the outage
-- the Postgres set avoids with CONCURRENTLY (see postgres/010..012).
--
-- MySQL DDL is not transactional: each statement below commits on its own and
-- sqlx's migration transaction is a no-op here. If the second or third
-- statement fails the earlier ones stay applied while the migration is *not*
-- recorded, and MySQL has no `CREATE INDEX IF NOT EXISTS` to make the re-run
-- idempotent — drop whichever of the two new indexes exists, then re-run.

CREATE INDEX `idx_traces_updated_at` ON `traces` (`updated_at`) ALGORITHM=INPLACE LOCK=NONE;
CREATE INDEX `idx_traces_created_at_id` ON `traces` (`created_at`, `id`) ALGORITHM=INPLACE LOCK=NONE;
DROP INDEX `idx_traces_created_at` ON `traces` ALGORITHM=INPLACE LOCK=NONE;
