-- Suffix the last two JSON columns, mirroring sqlite/009 (proposal D26).
--
-- See `sqlite/009` for what the suffix means. The same rename costs four times
-- the SQL here, because two kinds of dependent object hold the old name as
-- *text* rather than as a reference, and neither of them fails at rename time:
--
--   * A view's output column names are fixed when the view is created — the
--     `SELECT w.*` was expanded to a column list then. `RENAME COLUMN`
--     succeeds and leaves `current_workflows` still publishing a column called
--     `tags`, reading a table whose column is now `tags_json`. Nothing errors;
--     `SELECT * FROM current_workflows` simply keeps handing back the old
--     name, and the repository layer keeps decoding it, until someone
--     introspects the two and notices they disagree. `CREATE OR REPLACE VIEW`
--     cannot repair it ("cannot change name of view column"), so the views are
--     dropped and recreated.
--   * A plpgsql function body is a string re-parsed on each call. After the
--     rename, `enforce_workflows_active_immutable()` raises `record "old" has
--     no field "tags"` on the *next UPDATE of any active workflow* — the
--     immutability guard stops guarding and starts failing every write it
--     touches. The triggers reference their functions by OID and survive
--     intact, so only the two bodies are replaced.
--
-- `008_recreate_current_views.sql` is what the view half of this cost last
-- time: 004 dropped both views without `IF EXISTS`, so a run that did not
-- reach the recreate left a database that could never re-run 004. This file
-- carries no `-- no-transaction` marker, so sqlx wraps it in a transaction and
-- Postgres's transactional DDL makes it all-or-nothing; the `IF EXISTS` on the
-- drops is belt to that braces, and costs nothing.

DROP VIEW IF EXISTS current_workflows;
DROP VIEW IF EXISTS current_channels;

ALTER TABLE workflows RENAME COLUMN tags TO tags_json;
ALTER TABLE channels RENAME COLUMN methods TO methods_json;

-- Latest version per workflow_id (identical in shape to 001/004/008; only the
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

-- Same predicates as 005, reading the renamed columns.
CREATE OR REPLACE FUNCTION enforce_workflows_active_immutable() RETURNS TRIGGER AS $$
BEGIN
    IF OLD.status = 'active' AND NEW.status = 'active' AND (
           OLD.name              IS DISTINCT FROM NEW.name
        OR OLD.description       IS DISTINCT FROM NEW.description
        OR OLD.priority          IS DISTINCT FROM NEW.priority
        OR OLD.condition_json    IS DISTINCT FROM NEW.condition_json
        OR OLD.tasks_json        IS DISTINCT FROM NEW.tasks_json
        OR OLD.tags_json         IS DISTINCT FROM NEW.tags_json
        OR OLD.continue_on_error IS DISTINCT FROM NEW.continue_on_error
    ) THEN
        RAISE EXCEPTION 'Cannot modify content of active workflows';
    END IF;
    RETURN NEW;
END;
$$ LANGUAGE plpgsql;

CREATE OR REPLACE FUNCTION enforce_channels_active_immutable() RETURNS TRIGGER AS $$
BEGIN
    IF OLD.status = 'active' AND NEW.status = 'active' AND (
           OLD.name           IS DISTINCT FROM NEW.name
        OR OLD.description    IS DISTINCT FROM NEW.description
        OR OLD.channel_type   IS DISTINCT FROM NEW.channel_type
        OR OLD.protocol       IS DISTINCT FROM NEW.protocol
        OR OLD.methods_json   IS DISTINCT FROM NEW.methods_json
        OR OLD.route_pattern  IS DISTINCT FROM NEW.route_pattern
        OR OLD.topic          IS DISTINCT FROM NEW.topic
        OR OLD.consumer_group IS DISTINCT FROM NEW.consumer_group
        OR OLD.workflow_id    IS DISTINCT FROM NEW.workflow_id
        OR OLD.config_json    IS DISTINCT FROM NEW.config_json
    ) THEN
        RAISE EXCEPTION 'Cannot modify content of active channels';
    END IF;
    RETURN NEW;
END;
$$ LANGUAGE plpgsql;
