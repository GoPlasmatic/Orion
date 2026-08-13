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
-- an existing package still hashes to what its receipt recorded. It is why
-- this needs none of mysql/014's add-backfill-MODIFY dance — the column is
-- meant to be NULL, so there is nothing to backfill and no DEFAULT to state.
--
-- MySQL resolves a view's target list at creation, so `current_workflows`
-- (SELECT w.*) would keep serving the pre-loop column set — the trap
-- mysql/011 and /014 both document. Dropped and recreated below.
-- `current_channels` gains no column and is left alone. The immutability
-- trigger is recreated to guard the new content column. No DELIMITER: that is
-- a mysql-client directive sqlx cannot execute (see 001).

ALTER TABLE `workflows` ADD COLUMN `loop_json` text;

DROP VIEW IF EXISTS current_workflows;
CREATE VIEW current_workflows AS
SELECT w.*
FROM workflows w
INNER JOIN (
  SELECT workflow_id, MAX(version) AS max_version
  FROM workflows
  GROUP BY workflow_id
) latest ON w.workflow_id = latest.workflow_id AND w.version = latest.max_version;

DROP TRIGGER IF EXISTS trg_workflows_active_immutable;
CREATE TRIGGER trg_workflows_active_immutable
    BEFORE UPDATE ON workflows
    FOR EACH ROW
BEGIN
    IF OLD.status = 'active' AND NEW.status = 'active' AND (
           NOT (OLD.name              <=> NEW.name)
        OR NOT (OLD.description       <=> NEW.description)
        OR NOT (OLD.priority          <=> NEW.priority)
        OR NOT (OLD.condition_json    <=> NEW.condition_json)
        OR NOT (OLD.tasks_json        <=> NEW.tasks_json)
        OR NOT (OLD.tags_json         <=> NEW.tags_json)
        OR NOT (OLD.loop_json         <=> NEW.loop_json)
        OR NOT (OLD.continue_on_error <=> NEW.continue_on_error)
    ) THEN
        SIGNAL SQLSTATE '45000' SET MESSAGE_TEXT = 'Cannot modify content of active workflows';
    END IF;
END;
