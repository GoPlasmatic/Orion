-- Engine-managed loop over a workflow's task list (dataflow-rs 3.3).
--
-- A workflow carrying a `loop` runs its task list once per sweep instead of
-- once in total, with the engine maintaining a counter in `temp_data`. The
-- column stores the `LoopConfig` object verbatim as JSON — `{counter, init,
-- increment, max}` — or NULL for the overwhelming majority of workflows that
-- do not loop. Nullable rather than defaulted: `NULL` and `{}` are different
-- statements, and the engine's own field is `Option<LoopConfig>`, where
-- absent means "run the task list exactly once".
--
-- Nullable also keeps `content_hash` stable for every workflow that predates
-- this migration: the hashed projection omits the key when it is absent, so
-- an existing package still hashes to what its receipt recorded.
--
-- Postgres resolves a view's target list at creation, so `current_workflows`
-- (SELECT w.*) would keep serving the pre-loop column set — the trap
-- postgres/013 and /015 both document. Dropped and recreated below.
-- `current_channels` gains no column and is left alone. The immutability
-- function is replaced to guard the new content column; the trigger binding
-- references the function by OID and survives CREATE OR REPLACE FUNCTION.

ALTER TABLE workflows ADD COLUMN loop_json text;

DROP VIEW IF EXISTS current_workflows;
CREATE VIEW current_workflows AS
SELECT w.*
FROM workflows w
INNER JOIN (
  SELECT workflow_id, MAX(version) AS max_version
  FROM workflows
  GROUP BY workflow_id
) latest ON w.workflow_id = latest.workflow_id AND w.version = latest.max_version;

-- Same predicates as 013, plus the new content column.
CREATE OR REPLACE FUNCTION enforce_workflows_active_immutable() RETURNS TRIGGER AS $$
BEGIN
    IF OLD.status = 'active' AND NEW.status = 'active' AND (
           OLD.name              IS DISTINCT FROM NEW.name
        OR OLD.description       IS DISTINCT FROM NEW.description
        OR OLD.priority          IS DISTINCT FROM NEW.priority
        OR OLD.condition_json    IS DISTINCT FROM NEW.condition_json
        OR OLD.tasks_json        IS DISTINCT FROM NEW.tasks_json
        OR OLD.tags_json         IS DISTINCT FROM NEW.tags_json
        OR OLD.loop_json         IS DISTINCT FROM NEW.loop_json
        OR OLD.continue_on_error IS DISTINCT FROM NEW.continue_on_error
    ) THEN
        RAISE EXCEPTION 'Cannot modify content of active workflows';
    END IF;
    RETURN NEW;
END;
$$ LANGUAGE plpgsql;
