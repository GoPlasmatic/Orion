-- P-17/P-18: trace indexes that match the queries, and none that do not.
--
-- `idx_traces_channel_id` and `idx_traces_updated_at` had no reader:
-- nothing filters or sorts on `channel_id` alone, and `updated_at` changes on
-- every `update_status`/`complete` — the hottest writes in the schema. The
-- filtered list sorts by `(created_at, id)` after a channel or status filter,
-- so each filter gets an index that carries those keys.
DROP INDEX IF EXISTS idx_traces_channel_id;
DROP INDEX IF EXISTS idx_traces_updated_at;

CREATE INDEX idx_traces_channel_created_at_id
    ON traces (channel, created_at, id);
CREATE INDEX idx_traces_status_created_at_id
    ON traces (status, created_at, id);

-- The audit list filters by resource and then sorts the same way.
DROP INDEX IF EXISTS idx_audit_logs_resource;
CREATE INDEX idx_audit_logs_resource_created_at
    ON audit_logs (resource_type, resource_id, created_at, id);
