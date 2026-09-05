-- Cron channels — see migrations/sqlite/019_cron_scheduling.sql for the full
-- rationale. The same three tables and the same indexes in Postgres' dialect:
-- `bigint` for the integer columns as 004_bigint_columns made the other
-- entities, and the shared `update_updated_at_column()` trigger function
-- instead of three hand-written statement triggers.

CREATE TABLE IF NOT EXISTS "cron_schedule_state" (
    "channel_id" text NOT NULL PRIMARY KEY,
    "channel_version" bigint NOT NULL,
    "config_hash" text NOT NULL,
    "next_fire_at" timestamp NOT NULL,
    "paused_at" timestamp,
    "updated_at" timestamp NOT NULL DEFAULT CURRENT_TIMESTAMP
);

CREATE TABLE IF NOT EXISTS "cron_occurrences" (
    "id" text NOT NULL PRIMARY KEY,
    "channel_id" text NOT NULL,
    "channel_name" text NOT NULL,
    "channel_version" bigint NOT NULL,
    "executing_version" bigint,
    "workflow_id" text,
    "trigger" text NOT NULL DEFAULT 'cron',
    "scheduled_for" timestamp NOT NULL,
    "status" text NOT NULL DEFAULT 'pending',
    "attempt" bigint NOT NULL DEFAULT 0,
    "claimed_by" text,
    "claimed_until" timestamp,
    "singleton_key" text,
    "fencing_token" bigint,
    "trace_id" text,
    "error_message" text,
    "started_at" timestamp,
    "completed_at" timestamp,
    "created_at" timestamp NOT NULL DEFAULT CURRENT_TIMESTAMP,
    "updated_at" timestamp NOT NULL DEFAULT CURRENT_TIMESTAMP
);

CREATE UNIQUE INDEX IF NOT EXISTS "idx_cron_occurrences_identity"
    ON "cron_occurrences" ("channel_id", "scheduled_for");
CREATE INDEX IF NOT EXISTS "idx_cron_occurrences_due"
    ON "cron_occurrences" ("status", "scheduled_for");
CREATE INDEX IF NOT EXISTS "idx_cron_occurrences_claimed_until"
    ON "cron_occurrences" ("claimed_until");
CREATE INDEX IF NOT EXISTS "idx_cron_occurrences_created_at_id"
    ON "cron_occurrences" ("created_at", "id");
CREATE INDEX IF NOT EXISTS "idx_cron_occurrences_channel_created"
    ON "cron_occurrences" ("channel_id", "created_at");

CREATE TABLE IF NOT EXISTS "cron_singletons" (
    "singleton_key" text NOT NULL PRIMARY KEY,
    "occurrence_id" text NOT NULL,
    "holder" text NOT NULL,
    "fencing_token" bigint NOT NULL,
    "lease_until" timestamp NOT NULL,
    "updated_at" timestamp NOT NULL DEFAULT CURRENT_TIMESTAMP
);

CREATE INDEX IF NOT EXISTS "idx_cron_singletons_lease_until"
    ON "cron_singletons" ("lease_until");

CREATE TRIGGER trg_cron_schedule_state_updated_at
    BEFORE UPDATE ON cron_schedule_state
    FOR EACH ROW EXECUTE FUNCTION update_updated_at_column();

CREATE TRIGGER trg_cron_occurrences_updated_at
    BEFORE UPDATE ON cron_occurrences
    FOR EACH ROW EXECUTE FUNCTION update_updated_at_column();

CREATE TRIGGER trg_cron_singletons_updated_at
    BEFORE UPDATE ON cron_singletons
    FOR EACH ROW EXECUTE FUNCTION update_updated_at_column();
