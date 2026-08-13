-- Tags for channels and connectors (K6).
--
-- Workflows have carried `tags_json` + `?tag=` filtering since 0.x; channels
-- and connectors had no selection primitive at all, so "export the channels
-- and connectors belonging to package X" had no server-side spelling. Same
-- column shape and same wire contract as workflows: the column stores a JSON
-- array of strings, the API says `tags`.
--
-- SQLite view note: `current_channels` selects `c.*`, and SQLite resolves a
-- view's column list at query time, so the new column appears in the view
-- with no edit (the same property migrations/sqlite/009 relied on for the
-- rename). The active-immutability trigger is the opposite: its column
-- comparisons are explicit, so it is recreated here to guard the new content
-- column — without this, an active channel's tags could be edited in place,
-- which no other content column allows.

ALTER TABLE "channels" ADD COLUMN "tags_json" text NOT NULL DEFAULT '[]';
ALTER TABLE "connectors" ADD COLUMN "tags_json" text NOT NULL DEFAULT '[]';

DROP TRIGGER IF EXISTS trg_channels_active_immutable;
CREATE TRIGGER trg_channels_active_immutable
BEFORE UPDATE ON channels
WHEN OLD.status = 'active'
  AND NEW.status = 'active'
  AND (OLD.name != NEW.name
    OR OLD.description IS NOT NEW.description
    OR OLD.channel_type != NEW.channel_type
    OR OLD.protocol != NEW.protocol
    OR OLD.methods_json IS NOT NEW.methods_json
    OR OLD.route_pattern IS NOT NEW.route_pattern
    OR OLD.topic IS NOT NEW.topic
    OR OLD.consumer_group IS NOT NEW.consumer_group
    OR OLD.workflow_id IS NOT NEW.workflow_id
    OR OLD.config_json != NEW.config_json
    OR OLD.tags_json != NEW.tags_json)
BEGIN
  SELECT RAISE(ABORT, 'Cannot modify content of active channels');
END;
