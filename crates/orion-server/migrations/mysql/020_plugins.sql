-- Plugins (plugin.md): custom task functions as sandboxed WebAssembly
-- Components — see migrations/sqlite/017_plugins.sql for the full rationale.
-- The same pair of tables and lifecycle rules in MySQL's trigger dialect.
-- Key columns are varchar to fit index length limits: `plugin_id` is a
-- reverse-domain name the route layer caps well under 255, and a digest is
-- `sha256:` plus 64 hex characters. `longblob` for the component: the
-- default `plugins.max_component_bytes` is exactly a `mediumblob`'s ceiling,
-- and an operator may raise it.
--
-- MySQL DDL is not transactional: if this file fails part-way the tables
-- that were created stay and the triggers after the failure are absent until
-- it is re-run. Re-running is safe — every statement is `IF NOT EXISTS` or
-- a trigger that only exists once this file has completed.

CREATE TABLE IF NOT EXISTS `plugin_artifacts` (
    `digest` varchar(80) NOT NULL PRIMARY KEY,
    `bytes` longblob NOT NULL,
    `size` bigint NOT NULL,
    `created_at` datetime NOT NULL DEFAULT CURRENT_TIMESTAMP
);

CREATE TABLE IF NOT EXISTS `plugins` (
    `plugin_id` varchar(255) NOT NULL,
    `version` bigint NOT NULL,
    `status` varchar(16) NOT NULL DEFAULT 'draft',
    `digest` varchar(80) NOT NULL,
    `manifest_json` text NOT NULL,
    `tags_json` text NOT NULL,
    `created_at` datetime NOT NULL DEFAULT CURRENT_TIMESTAMP,
    `updated_at` datetime NOT NULL DEFAULT CURRENT_TIMESTAMP,
    PRIMARY KEY (`plugin_id`, `version`)
);

CREATE INDEX idx_plugins_status ON plugins(status);
CREATE INDEX idx_plugins_digest ON plugins(digest);

CREATE TRIGGER trg_plugins_updated_at
    BEFORE UPDATE ON plugins
    FOR EACH ROW
    SET NEW.updated_at = CURRENT_TIMESTAMP;

CREATE TRIGGER trg_plugins_single_draft
    BEFORE INSERT ON plugins
    FOR EACH ROW
BEGIN
    IF NEW.status = 'draft' THEN
        IF EXISTS (SELECT 1 FROM plugins WHERE plugin_id = NEW.plugin_id AND status = 'draft') THEN
            SIGNAL SQLSTATE '45000' SET MESSAGE_TEXT = 'Only one draft version allowed per plugin';
        END IF;
    END IF;
END;

CREATE TRIGGER trg_plugins_single_draft_update
    BEFORE UPDATE ON plugins
    FOR EACH ROW
BEGIN
    IF NEW.status = 'draft' THEN
        IF EXISTS (
            SELECT 1 FROM plugins
            WHERE plugin_id = NEW.plugin_id
              AND status = 'draft'
              AND version <> NEW.version
        ) THEN
            SIGNAL SQLSTATE '45000' SET MESSAGE_TEXT = 'Only one draft version allowed per plugin';
        END IF;
    END IF;
END;

CREATE TRIGGER trg_plugins_active_immutable
    BEFORE UPDATE ON plugins
    FOR EACH ROW
BEGIN
    IF OLD.status = 'active' AND NEW.status = 'active' AND (
           NOT (OLD.digest        <=> NEW.digest)
        OR NOT (OLD.manifest_json <=> NEW.manifest_json)
        OR NOT (OLD.tags_json     <=> NEW.tags_json)
    ) THEN
        SIGNAL SQLSTATE '45000' SET MESSAGE_TEXT = 'Cannot modify content of active plugins';
    END IF;
END;
