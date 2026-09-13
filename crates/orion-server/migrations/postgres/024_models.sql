-- Models: ONNX models referenced by an object-storage artifact — see
-- migrations/sqlite/020_models.sql for the full rationale, including why
-- `admission_json` and `stats_json` sit outside the active-immutability rule.
-- This is the same table and the same lifecycle rules in Postgres' dialect:
-- a partial unique index for the single draft (it covers INSERT and UPDATE
-- alike), the shared `update_updated_at_column()` function for `updated_at`,
-- and a plpgsql function for active immutability. `bigint` for the version,
-- as 004_bigint_columns made the other entities.

CREATE TABLE IF NOT EXISTS "models" (
    "model_id" text NOT NULL,
    "version" bigint NOT NULL,
    "status" text NOT NULL DEFAULT 'draft',
    "digest" text NOT NULL,
    "manifest_json" text NOT NULL,
    "artifact_json" text NOT NULL,
    "admission_json" text NOT NULL DEFAULT '{"state":"pending"}',
    "stats_json" text NULL,
    "tags_json" text NOT NULL DEFAULT '[]',
    "signature" text NULL,
    "created_at" timestamp NOT NULL DEFAULT CURRENT_TIMESTAMP,
    "updated_at" timestamp NOT NULL DEFAULT CURRENT_TIMESTAMP,
    PRIMARY KEY ("model_id", "version")
);

CREATE INDEX IF NOT EXISTS idx_models_status ON models(status);
CREATE INDEX IF NOT EXISTS idx_models_digest ON models(digest);

-- Single-draft enforcement via a partial unique index
CREATE UNIQUE INDEX idx_models_single_draft
    ON models (model_id) WHERE status = 'draft';

CREATE TRIGGER trg_models_updated_at
    BEFORE UPDATE ON models
    FOR EACH ROW EXECUTE FUNCTION update_updated_at_column();

-- Active rows are immutable: every content column is compared. `IS DISTINCT
-- FROM` so two NULL signatures compare equal. `admission_json` and
-- `stats_json` are absent on purpose — see the SQLite migration's header.
CREATE OR REPLACE FUNCTION enforce_models_active_immutable() RETURNS TRIGGER AS $$
BEGIN
    IF OLD.status = 'active' AND NEW.status = 'active' AND (
           OLD.digest        IS DISTINCT FROM NEW.digest
        OR OLD.manifest_json IS DISTINCT FROM NEW.manifest_json
        OR OLD.artifact_json IS DISTINCT FROM NEW.artifact_json
        OR OLD.tags_json     IS DISTINCT FROM NEW.tags_json
        OR OLD.signature     IS DISTINCT FROM NEW.signature
    ) THEN
        RAISE EXCEPTION 'Cannot modify content of active models';
    END IF;
    RETURN NEW;
END;
$$ LANGUAGE plpgsql;

CREATE TRIGGER trg_models_active_immutable
    BEFORE UPDATE ON models
    FOR EACH ROW EXECUTE FUNCTION enforce_models_active_immutable();

-- Restated, identically, after the trigger that names it: `schema_parity.rs`
-- reads the guarded columns from whatever follows the *last* mention of
-- `models_active_immutable` in the newest migration that has any, and the
-- trigger's EXECUTE FUNCTION line would otherwise be that mention — the same
-- arrangement `021_plugins.sql` documents.
CREATE OR REPLACE FUNCTION enforce_models_active_immutable() RETURNS TRIGGER AS $$
BEGIN
    IF OLD.status = 'active' AND NEW.status = 'active' AND (
           OLD.digest        IS DISTINCT FROM NEW.digest
        OR OLD.manifest_json IS DISTINCT FROM NEW.manifest_json
        OR OLD.artifact_json IS DISTINCT FROM NEW.artifact_json
        OR OLD.tags_json     IS DISTINCT FROM NEW.tags_json
        OR OLD.signature     IS DISTINCT FROM NEW.signature
    ) THEN
        RAISE EXCEPTION 'Cannot modify content of active models';
    END IF;
    RETURN NEW;
END;
$$ LANGUAGE plpgsql;
