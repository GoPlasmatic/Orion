-- Close two gaps in the channels active-immutability rule.
--
-- `trg_channels_active_immutable` compares every content column except
-- `priority` and `transport_config_json`, so an UPDATE against an active
-- channel row could change either one in place — no new version, no audit
-- row, and the engine keeps serving the definition it loaded until the next
-- reload picks the edit up. Both matter: `priority` decides which channel
-- wins an ambiguous route match, and `transport_config_json` carries the
-- Kafka transport settings.
--
-- The application layer never does this — `update_draft` and the import
-- upsert both constrain `status = 'draft'` — which is why the gap went
-- unnoticed. This is the database-level backstop the other content columns
-- have had since 001, restored for the two that were missed: `priority` when
-- the rule was first written, `transport_config_json` when the column was
-- added.
--
-- The equivalent workflows trigger has guarded `priority` all along; this is
-- also what makes the two rules symmetric, which `schema_parity.rs`
-- (`active_immutability_triggers_cover_every_content_column`) now requires.
--
-- `rollout_percentage` stays deliberately unguarded on `workflows`:
-- `PATCH /workflows/{id}/rollout` edits it on active rows by design.

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
    OR OLD.transport_config_json != NEW.transport_config_json
    OR OLD.workflow_id IS NOT NEW.workflow_id
    OR OLD.config_json != NEW.config_json
    OR OLD.priority != NEW.priority
    OR OLD.tags_json != NEW.tags_json)
BEGIN
  SELECT RAISE(ABORT, 'Cannot modify content of active channels');
END;
