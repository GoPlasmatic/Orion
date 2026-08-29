-- Close two gaps in the channels active-immutability rule.
--
-- See migrations/sqlite/015_channel_immutability_gaps.sql for the reasoning;
-- this is the same rule in MySQL's trigger dialect. `priority` and
-- `transport_config_json` were the two content columns the trigger did not
-- compare, so either could be edited in place on an active channel row.
--
-- MySQL DDL is not transactional: if this file fails between the DROP and the
-- CREATE the channels rule is absent until it is re-run. Re-running is safe —
-- the DROP is `IF EXISTS` and the CREATE is the whole rule.

DROP TRIGGER IF EXISTS trg_channels_active_immutable;
CREATE TRIGGER trg_channels_active_immutable
    BEFORE UPDATE ON channels
    FOR EACH ROW
BEGIN
    IF OLD.status = 'active' AND NEW.status = 'active' AND (
           NOT (OLD.name                  <=> NEW.name)
        OR NOT (OLD.description           <=> NEW.description)
        OR NOT (OLD.channel_type          <=> NEW.channel_type)
        OR NOT (OLD.protocol              <=> NEW.protocol)
        OR NOT (OLD.methods_json          <=> NEW.methods_json)
        OR NOT (OLD.route_pattern         <=> NEW.route_pattern)
        OR NOT (OLD.topic                 <=> NEW.topic)
        OR NOT (OLD.consumer_group        <=> NEW.consumer_group)
        OR NOT (OLD.transport_config_json <=> NEW.transport_config_json)
        OR NOT (OLD.workflow_id           <=> NEW.workflow_id)
        OR NOT (OLD.config_json           <=> NEW.config_json)
        OR NOT (OLD.priority              <=> NEW.priority)
        OR NOT (OLD.tags_json             <=> NEW.tags_json)
    ) THEN
        SIGNAL SQLSTATE '45000' SET MESSAGE_TEXT = 'Cannot modify content of active channels';
    END IF;
END;
