-- no-transaction
--
-- Keyset-pagination index for the trace list (proposal D8), part 2 of 3.
--
-- The default ordering is `created_at DESC` and the keyset cursor breaks ties
-- on `id`, so `(created_at, id)` turns the cursor predicate into a single index
-- range scan. It is a strict superset of the `idx_traces_created_at` that 012
-- drops: `created_at` is the leading column, so the retention delete's
-- `created_at < cutoff` is served by this index too.
--
-- `-- no-transaction` on line 1 and one statement per file: see the header of
-- 010_trace_updated_at_index.sql for why both are required, and for what to do
-- if a CONCURRENTLY build fails and leaves an INVALID index behind.

CREATE INDEX CONCURRENTLY IF NOT EXISTS "idx_traces_created_at_id" ON "traces" ("created_at", "id");
