-- Plugin signatures (`[plugins.trust]`): an optional detached signature over
-- a version's component digest, carried on the row so every node verifies it
-- again when it loads the version, not only the node that took the upload.
-- Expand-only: the column is nullable, a binary that predates it ignores it,
-- and a node with no `[plugins.trust]` keys stores whatever the upload sent
-- without checking it.
ALTER TABLE plugins ADD COLUMN IF NOT EXISTS "signature" text NULL;

-- Active rows are immutable: the rule restated with the new column, because
-- a signature is bound to the digest it signs and an active row may change
-- neither. The trigger keeps calling this function, so redefining it is the
-- whole change — and `schema_parity.rs` reads the guarded columns from the
-- last mention of `plugins_active_immutable`, which this is.
CREATE OR REPLACE FUNCTION enforce_plugins_active_immutable() RETURNS TRIGGER AS $$
BEGIN
    IF OLD.status = 'active' AND NEW.status = 'active' AND (
           OLD.digest        IS DISTINCT FROM NEW.digest
        OR OLD.manifest_json IS DISTINCT FROM NEW.manifest_json
        OR OLD.tags_json     IS DISTINCT FROM NEW.tags_json
        OR OLD.signature     IS DISTINCT FROM NEW.signature
    ) THEN
        RAISE EXCEPTION 'Cannot modify content of active plugins';
    END IF;
    RETURN NEW;
END;
$$ LANGUAGE plpgsql;
