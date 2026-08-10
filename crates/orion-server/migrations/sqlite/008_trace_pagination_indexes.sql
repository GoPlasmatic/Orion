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
-- cutoff` and turns the keyset predicate into one index range scan.
--
-- ## Locking, on SQLite specifically
--
-- SQLite has no online index build: `CREATE INDEX` holds the database-level
-- write lock for its duration, and there is no CONCURRENTLY to reach for. That
-- is accepted rather than worked around, because the SQLite deployment is
-- single-node and embedded — the same process that would be blocked is the one
-- running the migration, at startup, before the listener is bound. Postgres,
-- where a replica set keeps serving during a migration, builds these
-- CONCURRENTLY instead (postgres/010..012).
--
-- Unlike MySQL, SQLite DDL *is* transactional, so sqlx's migration transaction
-- makes this file all-or-nothing: a failure rolls back all three statements and
-- a re-run starts clean. No `IF NOT EXISTS` guards are needed.

CREATE INDEX "idx_traces_updated_at" ON "traces" ("updated_at");
CREATE INDEX "idx_traces_created_at_id" ON "traces" ("created_at", "id");
DROP INDEX "idx_traces_created_at";
