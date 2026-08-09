-- Tags for channels and connectors (K6).
--
-- Workflows have carried `tags_json` + `?tag=` filtering since 0.x; channels
-- and connectors had no selection primitive at all, so "export the channels
-- and connectors belonging to package X" had no server-side spelling. Same
-- column shape and same wire contract as workflows: the column stores a JSON
-- array of strings, the API says `tags`.
--
-- The add-backfill-MODIFY dance instead of `ADD COLUMN ... NOT NULL DEFAULT`:
-- a literal DEFAULT on a TEXT column needs the parenthesised expression form,
-- which only exists from MySQL 8.0.13 — the documented floor is "MySQL 8+",
-- so the sequence below works on every 8.x. No stored default is needed
-- afterwards: every INSERT path writes the column explicitly.
--
-- MySQL resolves a view's target list at creation, so `current_channels`
-- (SELECT c.*) would keep serving the pre-K6 column set — the exact trap
-- mysql/011 documents for the rename. Dropped and recreated below.
-- `current_workflows` gains no column and is left alone. The immutability
-- trigger is recreated to guard the new content column. No DELIMITER: that is
-- a mysql-client directive sqlx cannot execute (see 001).

ALTER TABLE `channels` ADD COLUMN `tags_json` text;
UPDATE `channels` SET `tags_json` = '[]' WHERE `tags_json` IS NULL;
ALTER TABLE `channels` MODIFY `tags_json` text NOT NULL;

ALTER TABLE `connectors` ADD COLUMN `tags_json` text;
UPDATE `connectors` SET `tags_json` = '[]' WHERE `tags_json` IS NULL;
ALTER TABLE `connectors` MODIFY `tags_json` text NOT NULL;

DROP VIEW IF EXISTS current_channels;
CREATE VIEW current_channels AS
SELECT c.*
FROM channels c
INNER JOIN (
  SELECT channel_id, MAX(version) AS max_version
  FROM channels
  GROUP BY channel_id
) latest ON c.channel_id = latest.channel_id AND c.version = latest.max_version;

DROP TRIGGER IF EXISTS trg_channels_active_immutable;
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
        OR NOT (OLD.tags_json      <=> NEW.tags_json)
    ) THEN
        SIGNAL SQLSTATE '45000' SET MESSAGE_TEXT = 'Cannot modify content of active channels';
    END IF;
END;
