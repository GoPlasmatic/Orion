-- Models: ONNX models referenced by an object-storage artifact — see
-- migrations/sqlite/020_models.sql for the full rationale, including why
-- `admission_json` and `stats_json` sit outside the active-immutability rule.
-- The same table and lifecycle rules in MySQL's trigger dialect. Key columns
-- are varchar to fit index length limits: `model_id` is a name the route
-- layer caps well under 255, and a digest is `sha256:` plus 64 hex
-- characters. The `text` columns carry no DEFAULT — MySQL refuses a literal
-- default on `text` — so the repository writes `admission_json` and
-- `tags_json` explicitly on every INSERT, on every backend, and the SQLite
-- and Postgres defaults are documentation rather than behaviour.
--
-- MySQL DDL is not transactional: if this file fails part-way the table that
-- was created stays and the triggers after the failure are absent until it
-- is re-run. Re-running is safe — every statement is `IF NOT EXISTS` or a
-- trigger that only exists once this file has completed.

CREATE TABLE IF NOT EXISTS `models` (
    `model_id` varchar(255) NOT NULL,
    `version` bigint NOT NULL,
    `status` varchar(16) NOT NULL DEFAULT 'draft',
    `digest` varchar(80) NOT NULL,
    `manifest_json` text NOT NULL,
    `artifact_json` text NOT NULL,
    `admission_json` text NOT NULL,
    `stats_json` text NULL,
    `tags_json` text NOT NULL,
    `signature` text NULL,
    `created_at` datetime NOT NULL DEFAULT CURRENT_TIMESTAMP,
    `updated_at` datetime NOT NULL DEFAULT CURRENT_TIMESTAMP,
    PRIMARY KEY (`model_id`, `version`)
);

CREATE INDEX idx_models_status ON models(status);
CREATE INDEX idx_models_digest ON models(digest);

CREATE TRIGGER trg_models_updated_at
    BEFORE UPDATE ON models
    FOR EACH ROW
    SET NEW.updated_at = CURRENT_TIMESTAMP;

CREATE TRIGGER trg_models_single_draft
    BEFORE INSERT ON models
    FOR EACH ROW
BEGIN
    IF NEW.status = 'draft' THEN
        IF EXISTS (SELECT 1 FROM models WHERE model_id = NEW.model_id AND status = 'draft') THEN
            SIGNAL SQLSTATE '45000' SET MESSAGE_TEXT = 'Only one draft version allowed per model';
        END IF;
    END IF;
END;

CREATE TRIGGER trg_models_single_draft_update
    BEFORE UPDATE ON models
    FOR EACH ROW
BEGIN
    IF NEW.status = 'draft' THEN
        IF EXISTS (
            SELECT 1 FROM models
            WHERE model_id = NEW.model_id
              AND status = 'draft'
              AND version <> NEW.version
        ) THEN
            SIGNAL SQLSTATE '45000' SET MESSAGE_TEXT = 'Only one draft version allowed per model';
        END IF;
    END IF;
END;

-- Active rows are immutable: every content column is compared. `<=>` is the
-- null-safe equality, so two NULL signatures compare equal. `admission_json`
-- and `stats_json` are absent on purpose — see the SQLite migration's header.
CREATE TRIGGER trg_models_active_immutable
    BEFORE UPDATE ON models
    FOR EACH ROW
BEGIN
    IF OLD.status = 'active' AND NEW.status = 'active' AND (
           NOT (OLD.digest        <=> NEW.digest)
        OR NOT (OLD.manifest_json <=> NEW.manifest_json)
        OR NOT (OLD.artifact_json <=> NEW.artifact_json)
        OR NOT (OLD.tags_json     <=> NEW.tags_json)
        OR NOT (OLD.signature     <=> NEW.signature)
    ) THEN
        SIGNAL SQLSTATE '45000' SET MESSAGE_TEXT = 'Cannot modify content of active models';
    END IF;
END;
