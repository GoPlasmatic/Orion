-- Enforce the single-draft invariant on UPDATE, not just INSERT (proposal D9).
--
-- Postgres enforces this with a partial unique index, which covers INSERT and
-- UPDATE alike. SQLite and MySQL had BEFORE INSERT triggers only, so
-- `UPDATE workflows SET status='draft' ...` produced a second draft on two of
-- three backends. `draft_query` then feeds `fetch_optional_as`, which silently
-- takes an arbitrary one of them.
--
-- Additive and strictly narrowing: it only rejects writes that were already
-- invalid under the documented invariant.

CREATE TRIGGER trg_workflows_single_draft_update
    BEFORE UPDATE ON workflows
    FOR EACH ROW
BEGIN
    IF NEW.status = 'draft' THEN
        IF EXISTS (
            SELECT 1 FROM workflows
            WHERE workflow_id = NEW.workflow_id
              AND status = 'draft'
              AND version <> NEW.version
        ) THEN
            SIGNAL SQLSTATE '45000' SET MESSAGE_TEXT = 'Only one draft version allowed per workflow';
        END IF;
    END IF;
END;

CREATE TRIGGER trg_channels_single_draft_update
    BEFORE UPDATE ON channels
    FOR EACH ROW
BEGIN
    IF NEW.status = 'draft' THEN
        IF EXISTS (
            SELECT 1 FROM channels
            WHERE channel_id = NEW.channel_id
              AND status = 'draft'
              AND version <> NEW.version
        ) THEN
            SIGNAL SQLSTATE '45000' SET MESSAGE_TEXT = 'Only one draft version allowed per channel';
        END IF;
    END IF;
END;
