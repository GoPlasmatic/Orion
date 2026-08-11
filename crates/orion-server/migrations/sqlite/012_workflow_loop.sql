-- Engine-managed loop over a workflow's task list (dataflow-rs 3.3).
--
-- A workflow carrying a `loop` runs its task list once per sweep instead of
-- once in total, with the engine maintaining a counter in `temp_data`. The
-- column stores the `LoopConfig` object verbatim as JSON — `{counter, init,
-- increment, max}` — or NULL for the overwhelming majority of workflows that
-- do not loop. Nullable rather than defaulted: `NULL` and `{}` are different
-- statements, and the engine's own field is `Option<LoopConfig>`, where
-- absent means "run the task list exactly once, on a code path that carries
-- no loop overhead".
--
-- Nullable also keeps `content_hash` stable for every workflow that predates
-- this migration: the hashed projection omits the key when it is absent, so
-- an existing package still hashes to what its receipt recorded, and
-- `apply` stays a no-op rather than tripping the content-immutability 409.
--
-- SQLite view note: `current_workflows` selects `w.*`, and SQLite resolves a
-- view's column list at query time, so the new column appears in the view
-- with no edit (the same property migrations/sqlite/009 and 011 relied on).
-- The active-immutability trigger is the opposite: its column comparisons are
-- explicit, so it is recreated here to guard the new content column. Without
-- this, an active workflow's loop bound could be edited in place — turning a
-- 10-sweep loop into a 10-million-sweep one without a new version, which no
-- other content column allows.

ALTER TABLE "workflows" ADD COLUMN "loop_json" text;

DROP TRIGGER IF EXISTS trg_workflows_active_immutable;
CREATE TRIGGER trg_workflows_active_immutable
BEFORE UPDATE ON workflows
WHEN OLD.status = 'active'
  AND NEW.status = 'active'
  AND (OLD.name != NEW.name
    OR OLD.description IS NOT NEW.description
    OR OLD.priority != NEW.priority
    OR OLD.condition_json != NEW.condition_json
    OR OLD.tasks_json != NEW.tasks_json
    OR OLD.tags_json != NEW.tags_json
    OR OLD.loop_json IS NOT NEW.loop_json
    OR OLD.continue_on_error != NEW.continue_on_error)
BEGIN
  SELECT RAISE(ABORT, 'Cannot modify content of active workflows');
END;
