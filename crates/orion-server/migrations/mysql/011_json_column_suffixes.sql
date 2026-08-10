-- Suffix the last two JSON columns, mirroring sqlite/009 (proposal D26).
--
-- See `sqlite/009` for what the suffix means. MySQL, like Postgres, resolves
-- `SELECT w.*` into a stored column list at CREATE VIEW time and stores
-- trigger bodies as text, so both break here — more loudly than on Postgres,
-- and in the other order:
--
--   * The views do not go stale, they go *invalid*: after the rename, any
--     `SELECT * FROM current_workflows` fails with ER_VIEW_INVALID (1356,
--     "references invalid table(s) or column(s)"), and the view disappears
--     from `information_schema.COLUMNS` entirely. Every repository read that
--     goes through `current_workflows` / `current_channels` — which is all of
--     the latest-version reads — stops working.
--   * The active-immutability triggers from 004 keep their old text, so the
--     next UPDATE of any workflow or channel row fails with ER_BAD_FIELD_ERROR
--     (1054, "Unknown column 'tags' in 'OLD'"). MySQL does not re-check
--     trigger bodies at ALTER time, so nothing warns at rename time.
--
-- 007's note that MySQL "has no view-dependency restriction on ALTER" is true
-- of `MODIFY`, which changes a type and leaves the name alone. It does not
-- carry over to `RENAME COLUMN`, which is the whole difficulty of this file.
--
-- Unlike postgres/013 this cannot be atomic: MySQL commits implicitly on every
-- DDL statement, so a run that dies partway leaves the schema half-moved and
-- `_sqlx_migrations` without the row, and the retry fails on the rename
-- ("Unknown column 'tags'"). That is the same exposure every MySQL migration
-- in this directory carries, and the recovery is the one in the upgrade guide:
-- finish the file by hand, then let sqlx re-run it. It is also close to
-- theoretical here — MySQL storage is new in 1.0.0 (see
-- `docs/src/reference/support.md`), so there is no 0.x MySQL database for this
-- to migrate; in practice it runs once, against empty tables, at first start.

DROP VIEW IF EXISTS current_workflows;
DROP VIEW IF EXISTS current_channels;
DROP TRIGGER IF EXISTS trg_workflows_active_immutable;
DROP TRIGGER IF EXISTS trg_channels_active_immutable;

ALTER TABLE `workflows` RENAME COLUMN `tags` TO `tags_json`;
ALTER TABLE `channels` RENAME COLUMN `methods` TO `methods_json`;

-- Latest version per workflow_id (identical in shape to 001; only the
-- underlying column name has moved).
CREATE VIEW current_workflows AS
SELECT w.*
FROM workflows w
INNER JOIN (
  SELECT workflow_id, MAX(version) AS max_version
  FROM workflows
  GROUP BY workflow_id
) latest ON w.workflow_id = latest.workflow_id AND w.version = latest.max_version;

-- Latest version per channel_id
CREATE VIEW current_channels AS
SELECT c.*
FROM channels c
INNER JOIN (
  SELECT channel_id, MAX(version) AS max_version
  FROM channels
  GROUP BY channel_id
) latest ON c.channel_id = latest.channel_id AND c.version = latest.max_version;

-- Same predicates as 004, reading the renamed columns. No DELIMITER: that is a
-- mysql-client directive sqlx cannot execute, and the server parses each
-- compound CREATE TRIGGER as one statement regardless (see 001).
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
        OR NOT (OLD.continue_on_error <=> NEW.continue_on_error)
    ) THEN
        SIGNAL SQLSTATE '45000' SET MESSAGE_TEXT = 'Cannot modify content of active workflows';
    END IF;
END;

CREATE TRIGGER trg_channels_active_immutable
    BEFORE UPDATE ON channels
    FOR EACH ROW
BEGIN
    IF OLD.status = 'active' AND NEW.status = 'active' AND (
           NOT (OLD.name           <=> NEW.name)
        OR NOT (OLD.description    <=> NEW.description)
        OR NOT (OLD.channel_type   <=> NEW.channel_type)
        OR NOT (OLD.protocol       <=> NEW.protocol)
        OR NOT (OLD.methods_json   <=> NEW.methods_json)
        OR NOT (OLD.route_pattern  <=> NEW.route_pattern)
        OR NOT (OLD.topic          <=> NEW.topic)
        OR NOT (OLD.consumer_group <=> NEW.consumer_group)
        OR NOT (OLD.workflow_id    <=> NEW.workflow_id)
        OR NOT (OLD.config_json    <=> NEW.config_json)
    ) THEN
        SIGNAL SQLSTATE '45000' SET MESSAGE_TEXT = 'Cannot modify content of active channels';
    END IF;
END;
