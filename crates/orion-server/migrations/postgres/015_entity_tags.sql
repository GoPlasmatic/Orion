-- Tags for channels and connectors (K6).
--
-- Workflows have carried `tags_json` + `?tag=` filtering since 0.x; channels
-- and connectors had no selection primitive at all, so "export the channels
-- and connectors belonging to package X" had no server-side spelling. Same
-- column shape and same wire contract as workflows: the column stores a JSON
-- array of strings, the API says `tags`.
--
-- Postgres resolves a view's target list at creation, so `current_channels`
-- (SELECT c.*) would keep serving the pre-K6 column set — the exact trap
-- postgres/013 documents for the rename. Dropped and recreated below.
-- `current_workflows` gains no column and is left alone. The immutability
-- function is replaced to guard the new content column; the trigger binding
-- survives a CREATE OR REPLACE FUNCTION.

ALTER TABLE channels ADD COLUMN tags_json text NOT NULL DEFAULT '[]';
ALTER TABLE connectors ADD COLUMN tags_json text NOT NULL DEFAULT '[]';

DROP VIEW IF EXISTS current_channels;
CREATE VIEW current_channels AS
SELECT c.*
FROM channels c
INNER JOIN (
  SELECT channel_id, MAX(version) AS max_version
  FROM channels
  GROUP BY channel_id
) latest ON c.channel_id = latest.channel_id AND c.version = latest.max_version;

-- Same predicates as 013, plus the new content column.
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
        OR OLD.tags_json      IS DISTINCT FROM NEW.tags_json
    ) THEN
        RAISE EXCEPTION 'Cannot modify content of active channels';
    END IF;
    RETURN NEW;
END;
$$ LANGUAGE plpgsql;
