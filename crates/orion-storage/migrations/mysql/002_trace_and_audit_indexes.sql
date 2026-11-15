-- P-17/P-18: trace indexes that match the queries, and none that do not.
--
-- `idx_traces_channel_id` and `idx_traces_updated_at` had no reader:
-- nothing filters or sorts on `channel_id` alone, and `updated_at` changes on
-- every `update_status`/`complete` — the hottest writes in the schema. The
-- filtered list sorts by `(created_at, id)` after a channel or status filter,
-- so each filter gets an index that carries those keys.
ALTER TABLE `traces`
    DROP INDEX `idx_traces_channel_id`,
    DROP INDEX `idx_traces_updated_at`;

ALTER TABLE `traces`
    ADD INDEX `idx_traces_channel_created_at_id` (`channel`, `created_at`, `id`),
    ADD INDEX `idx_traces_status_created_at_id` (`status`, `created_at`, `id`);

-- The audit list filters by resource and then sorts the same way.
ALTER TABLE `audit_logs`
    DROP INDEX `idx_audit_logs_resource`;

ALTER TABLE `audit_logs`
    ADD INDEX `idx_audit_logs_resource_created_at`
        (`resource_type`, `resource_id`, `created_at`, `id`);
