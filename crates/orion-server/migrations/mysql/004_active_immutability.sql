-- Active-immutability parity with the sqlite backend (multi-instance-ha 0.8).
-- sqlite/001 ships triggers that reject content changes to active workflow /
-- channel versions; mysql had none. The repository layer already scopes its
-- UPDATEs by status, so these are defence-in-depth against raw SQL and future
-- code paths. NOT (OLD.col <=> NEW.col) is MySQL's null-safe inequality,
-- mirroring postgres's IS DISTINCT FROM.

CREATE TRIGGER trg_workflows_active_immutable
    BEFORE UPDATE ON workflows
    FOR EACH ROW
BEGIN
    IF OLD.status = 'active' AND NEW.status = 'active' AND (
           NOT (OLD.name              <=> NEW.name)
        OR NOT (OLD.description       <=> NEW.description)
        OR NOT (OLD.priority          <=> NEW.priority)
        OR NOT (OLD.condition_json    <=> NEW.condition_json)
        OR NOT (OLD.tasks_json        <=> NEW.tasks_json)
        OR NOT (OLD.tags              <=> NEW.tags)
        OR NOT (OLD.continue_on_error <=> NEW.continue_on_error)
    ) THEN
        SIGNAL SQLSTATE '45000' SET MESSAGE_TEXT = 'Cannot modify content of active workflows';
    END IF;
END;

CREATE TRIGGER trg_channels_active_immutable
    BEFORE UPDATE ON channels
    FOR EACH ROW
BEGIN
    IF OLD.status = 'active' AND NEW.status = 'active' AND (
           NOT (OLD.name           <=> NEW.name)
        OR NOT (OLD.description    <=> NEW.description)
        OR NOT (OLD.channel_type   <=> NEW.channel_type)
        OR NOT (OLD.protocol       <=> NEW.protocol)
        OR NOT (OLD.methods        <=> NEW.methods)
        OR NOT (OLD.route_pattern  <=> NEW.route_pattern)
        OR NOT (OLD.topic          <=> NEW.topic)
        OR NOT (OLD.consumer_group <=> NEW.consumer_group)
        OR NOT (OLD.workflow_id    <=> NEW.workflow_id)
        OR NOT (OLD.config_json    <=> NEW.config_json)
    ) THEN
        SIGNAL SQLSTATE '45000' SET MESSAGE_TEXT = 'Cannot modify content of active channels';
    END IF;
END;
