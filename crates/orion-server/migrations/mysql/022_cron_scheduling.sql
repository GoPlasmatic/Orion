-- Cron channels — see migrations/sqlite/019_cron_scheduling.sql for the full
-- rationale. The same three tables and indexes in MySQL's dialect.
--
-- Key columns are varchar to fit index length limits: `channel_id` is a
-- validated identifier the API caps well under 255, `id` is a UUID, and a
-- singleton key is capped at 128 characters at authoring time.
--
-- MySQL DDL is not transactional: if this file fails part-way the tables it
-- created stay and the triggers after the failure are absent until it is
-- re-run. Re-running is safe — every statement is `IF NOT EXISTS` or a trigger
-- that only exists once this file has completed.

CREATE TABLE IF NOT EXISTS `cron_schedule_state` (
    `channel_id` varchar(255) NOT NULL PRIMARY KEY,
    `channel_version` bigint NOT NULL,
    `config_hash` varchar(80) NOT NULL,
    `next_fire_at` datetime NOT NULL,
    `paused_at` datetime,
    `updated_at` datetime NOT NULL DEFAULT CURRENT_TIMESTAMP
);

CREATE TABLE IF NOT EXISTS `cron_occurrences` (
    `id` varchar(64) NOT NULL PRIMARY KEY,
    `channel_id` varchar(255) NOT NULL,
    `channel_name` varchar(255) NOT NULL,
    `channel_version` bigint NOT NULL,
    `executing_version` bigint,
    `workflow_id` varchar(255),
    `trigger` varchar(16) NOT NULL DEFAULT 'cron',
    `scheduled_for` datetime NOT NULL,
    `status` varchar(24) NOT NULL DEFAULT 'pending',
    `attempt` bigint NOT NULL DEFAULT 0,
    `claimed_by` varchar(64),
    `claimed_until` datetime,
    `singleton_key` varchar(128),
    `fencing_token` bigint,
    `trace_id` varchar(64),
    `error_message` text,
    `started_at` datetime,
    `completed_at` datetime,
    `created_at` datetime NOT NULL DEFAULT CURRENT_TIMESTAMP,
    `updated_at` datetime NOT NULL DEFAULT CURRENT_TIMESTAMP
);

CREATE UNIQUE INDEX idx_cron_occurrences_identity
    ON cron_occurrences (channel_id, scheduled_for);
CREATE INDEX idx_cron_occurrences_due
    ON cron_occurrences (status, scheduled_for);
CREATE INDEX idx_cron_occurrences_claimed_until
    ON cron_occurrences (claimed_until);
CREATE INDEX idx_cron_occurrences_created_at_id
    ON cron_occurrences (created_at, id);
CREATE INDEX idx_cron_occurrences_channel_created
    ON cron_occurrences (channel_id, created_at);

CREATE TABLE IF NOT EXISTS `cron_singletons` (
    `singleton_key` varchar(128) NOT NULL PRIMARY KEY,
    `occurrence_id` varchar(64) NOT NULL,
    `holder` varchar(64) NOT NULL,
    `fencing_token` bigint NOT NULL,
    `lease_until` datetime NOT NULL,
    `updated_at` datetime NOT NULL DEFAULT CURRENT_TIMESTAMP
);

CREATE INDEX idx_cron_singletons_lease_until
    ON cron_singletons (lease_until);

CREATE TRIGGER trg_cron_schedule_state_updated_at
    BEFORE UPDATE ON cron_schedule_state
    FOR EACH ROW
    SET NEW.updated_at = CURRENT_TIMESTAMP;

CREATE TRIGGER trg_cron_occurrences_updated_at
    BEFORE UPDATE ON cron_occurrences
    FOR EACH ROW
    SET NEW.updated_at = CURRENT_TIMESTAMP;

CREATE TRIGGER trg_cron_singletons_updated_at
    BEFORE UPDATE ON cron_singletons
    FOR EACH ROW
    SET NEW.updated_at = CURRENT_TIMESTAMP;
