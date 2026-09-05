-- Plugin signatures (`[plugins.trust]`): an optional detached signature over
-- a version's component digest, carried on the row so every node verifies it
-- again when it loads the version, not only the node that took the upload.
-- Expand-only: the column is nullable, a binary that predates it ignores it,
-- and a node with no `[plugins.trust]` keys stores whatever the upload sent
-- without checking it.
ALTER TABLE plugins ADD COLUMN `signature` text NULL;

-- Active rows are immutable: the rule restated with the new column, because
-- a signature is bound to the digest it signs and an active row may change
-- neither. `<=>` is the null-safe equality, so two NULLs compare equal.
DROP TRIGGER IF EXISTS trg_plugins_active_immutable;
CREATE TRIGGER trg_plugins_active_immutable
    BEFORE UPDATE ON plugins
    FOR EACH ROW
BEGIN
    IF OLD.status = 'active' AND NEW.status = 'active' AND (
           NOT (OLD.digest        <=> NEW.digest)
        OR NOT (OLD.manifest_json <=> NEW.manifest_json)
        OR NOT (OLD.tags_json     <=> NEW.tags_json)
        OR NOT (OLD.signature     <=> NEW.signature)
    ) THEN
        SIGNAL SQLSTATE '45000' SET MESSAGE_TEXT = 'Cannot modify content of active plugins';
    END IF;
END;
