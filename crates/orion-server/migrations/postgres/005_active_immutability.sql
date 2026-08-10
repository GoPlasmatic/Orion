-- Active-immutability parity with the sqlite backend (multi-instance-ha 0.8).
-- sqlite/001 ships triggers that reject content changes to active workflow /
-- channel versions; postgres had none. The repository layer already scopes
-- its UPDATEs by status, so these are defence-in-depth against raw SQL and
-- future code paths — the predicate mirrors sqlite's trigger WHEN clauses
-- (IS DISTINCT FROM unifies sqlite's != / IS NOT split for nullable columns).

CREATE OR REPLACE FUNCTION enforce_workflows_active_immutable() RETURNS TRIGGER AS $$
BEGIN
    IF OLD.status = 'active' AND NEW.status = 'active' AND (
           OLD.name              IS DISTINCT FROM NEW.name
        OR OLD.description       IS DISTINCT FROM NEW.description
        OR OLD.priority          IS DISTINCT FROM NEW.priority
        OR OLD.condition_json    IS DISTINCT FROM NEW.condition_json
        OR OLD.tasks_json        IS DISTINCT FROM NEW.tasks_json
        OR OLD.tags              IS DISTINCT FROM NEW.tags
        OR OLD.continue_on_error IS DISTINCT FROM NEW.continue_on_error
    ) THEN
        RAISE EXCEPTION 'Cannot modify content of active workflows';
    END IF;
    RETURN NEW;
END;
$$ LANGUAGE plpgsql;

CREATE TRIGGER trg_workflows_active_immutable
    BEFORE UPDATE ON workflows
    FOR EACH ROW EXECUTE FUNCTION enforce_workflows_active_immutable();

CREATE OR REPLACE FUNCTION enforce_channels_active_immutable() RETURNS TRIGGER AS $$
BEGIN
    IF OLD.status = 'active' AND NEW.status = 'active' AND (
           OLD.name           IS DISTINCT FROM NEW.name
        OR OLD.description    IS DISTINCT FROM NEW.description
        OR OLD.channel_type   IS DISTINCT FROM NEW.channel_type
        OR OLD.protocol       IS DISTINCT FROM NEW.protocol
        OR OLD.methods        IS DISTINCT FROM NEW.methods
        OR OLD.route_pattern  IS DISTINCT FROM NEW.route_pattern
        OR OLD.topic          IS DISTINCT FROM NEW.topic
        OR OLD.consumer_group IS DISTINCT FROM NEW.consumer_group
        OR OLD.workflow_id    IS DISTINCT FROM NEW.workflow_id
        OR OLD.config_json    IS DISTINCT FROM NEW.config_json
    ) THEN
        RAISE EXCEPTION 'Cannot modify content of active channels';
    END IF;
    RETURN NEW;
END;
$$ LANGUAGE plpgsql;

CREATE TRIGGER trg_channels_active_immutable
    BEFORE UPDATE ON channels
    FOR EACH ROW EXECUTE FUNCTION enforce_channels_active_immutable();
