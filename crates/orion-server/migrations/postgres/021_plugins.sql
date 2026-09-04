-- Plugins (plugin.md): custom task functions as sandboxed WebAssembly
-- Components — see migrations/sqlite/017_plugins.sql for the full rationale.
-- This is the same pair of tables and the same lifecycle rules in Postgres'
-- dialect: a partial unique index for the single draft (it covers INSERT
-- and UPDATE alike), the shared `update_updated_at_column()` function for
-- `updated_at`, and a plpgsql function for active immutability. `bigint`
-- for the integer columns, as 004_bigint_columns made the other entities.

CREATE TABLE IF NOT EXISTS "plugin_artifacts" (
    "digest" text NOT NULL PRIMARY KEY,
    "bytes" bytea NOT NULL,
    "size" bigint NOT NULL,
    "created_at" timestamp NOT NULL DEFAULT CURRENT_TIMESTAMP
);

CREATE TABLE IF NOT EXISTS "plugins" (
    "plugin_id" text NOT NULL,
    "version" bigint NOT NULL,
    "status" text NOT NULL DEFAULT 'draft',
    "digest" text NOT NULL,
    "manifest_json" text NOT NULL,
    "tags_json" text NOT NULL DEFAULT '[]',
    "created_at" timestamp NOT NULL DEFAULT CURRENT_TIMESTAMP,
    "updated_at" timestamp NOT NULL DEFAULT CURRENT_TIMESTAMP,
    PRIMARY KEY ("plugin_id", "version")
);

CREATE INDEX IF NOT EXISTS idx_plugins_status ON plugins(status);
CREATE INDEX IF NOT EXISTS idx_plugins_digest ON plugins(digest);

-- Single-draft enforcement via a partial unique index
CREATE UNIQUE INDEX idx_plugins_single_draft
    ON plugins (plugin_id) WHERE status = 'draft';

CREATE TRIGGER trg_plugins_updated_at
    BEFORE UPDATE ON plugins
    FOR EACH ROW EXECUTE FUNCTION update_updated_at_column();

CREATE OR REPLACE FUNCTION enforce_plugins_active_immutable() RETURNS TRIGGER AS $$
BEGIN
    IF OLD.status = 'active' AND NEW.status = 'active' AND (
           OLD.digest        IS DISTINCT FROM NEW.digest
        OR OLD.manifest_json IS DISTINCT FROM NEW.manifest_json
        OR OLD.tags_json     IS DISTINCT FROM NEW.tags_json
    ) THEN
        RAISE EXCEPTION 'Cannot modify content of active plugins';
    END IF;
    RETURN NEW;
END;
$$ LANGUAGE plpgsql;

CREATE TRIGGER trg_plugins_active_immutable
    BEFORE UPDATE ON plugins
    FOR EACH ROW EXECUTE FUNCTION enforce_plugins_active_immutable();

-- Restated, identically, after the trigger that names it: `schema_parity.rs`
-- reads the guarded columns from whatever follows the *last* mention of
-- `plugins_active_immutable` in the newest migration that has any, and the
-- trigger's EXECUTE FUNCTION line would otherwise be that mention. The two
-- older entities got this for free because their rules were last touched by
-- a migration that only redefined the function.
CREATE OR REPLACE FUNCTION enforce_plugins_active_immutable() RETURNS TRIGGER AS $$
BEGIN
    IF OLD.status = 'active' AND NEW.status = 'active' AND (
           OLD.digest        IS DISTINCT FROM NEW.digest
        OR OLD.manifest_json IS DISTINCT FROM NEW.manifest_json
        OR OLD.tags_json     IS DISTINCT FROM NEW.tags_json
    ) THEN
        RAISE EXCEPTION 'Cannot modify content of active plugins';
    END IF;
    RETURN NEW;
END;
$$ LANGUAGE plpgsql;
