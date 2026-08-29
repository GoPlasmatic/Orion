-- Close two gaps in the channels active-immutability rule.
--
-- See migrations/sqlite/015_channel_immutability_gaps.sql for the reasoning;
-- this is the same rule in plpgsql. `priority` and `transport_config_json`
-- were the two content columns `enforce_channels_active_immutable()` did not
-- compare, so either could be edited in place on an active channel row.
--
-- CREATE OR REPLACE FUNCTION is idempotent and the trigger already points at
-- this function by name, so no trigger DDL is needed here.

CREATE OR REPLACE FUNCTION enforce_channels_active_immutable() RETURNS TRIGGER AS $$
BEGIN
    IF OLD.status = 'active' AND NEW.status = 'active' AND (
           OLD.name                  IS DISTINCT FROM NEW.name
        OR OLD.description           IS DISTINCT FROM NEW.description
        OR OLD.channel_type          IS DISTINCT FROM NEW.channel_type
        OR OLD.protocol              IS DISTINCT FROM NEW.protocol
        OR OLD.methods_json          IS DISTINCT FROM NEW.methods_json
        OR OLD.route_pattern         IS DISTINCT FROM NEW.route_pattern
        OR OLD.topic                 IS DISTINCT FROM NEW.topic
        OR OLD.consumer_group        IS DISTINCT FROM NEW.consumer_group
        OR OLD.transport_config_json IS DISTINCT FROM NEW.transport_config_json
        OR OLD.workflow_id           IS DISTINCT FROM NEW.workflow_id
        OR OLD.config_json           IS DISTINCT FROM NEW.config_json
        OR OLD.priority              IS DISTINCT FROM NEW.priority
        OR OLD.tags_json             IS DISTINCT FROM NEW.tags_json
    ) THEN
        RAISE EXCEPTION 'Cannot modify content of active channels';
    END IF;
    RETURN NEW;
END;
$$ LANGUAGE plpgsql;
