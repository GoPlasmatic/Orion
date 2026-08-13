-- no-transaction
--
-- Drop the index 011 supersedes (proposal D8), part 3 of 3.
--
-- `idx_traces_created_at` is a proper prefix of `idx_traces_created_at_id`, so
-- keeping it buys nothing and costs a second B-tree write on every trace
-- insert and every status update — on the highest-write table in the schema.
--
-- Dropped CONCURRENTLY: a plain `DROP INDEX` takes an ACCESS EXCLUSIVE lock,
-- and while the drop itself is a catalog update, *waiting* for that lock queues
-- every subsequent query behind the transaction currently reading the table.
-- Like CREATE INDEX CONCURRENTLY it cannot run inside a transaction block,
-- hence the `-- no-transaction` marker on line 1 and its own file.
--
-- `IF EXISTS` so a re-run after a partially applied 010/011/012 sequence is a
-- no-op rather than a hard failure.

DROP INDEX CONCURRENTLY IF EXISTS "idx_traces_created_at";
