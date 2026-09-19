-- Bounded concurrency for cron channels: `concurrency.slots` lets up to N
-- occurrences of one singleton key run at once. Slot 0 stays the
-- `cron_singletons` row it has always been — an older binary and this one
-- contend for it with the same statements, so a key with one slot behaves
-- exactly as before in every version mix — and slots 1..N-1 live here.
-- Expand-only: an older binary neither reads nor writes this table or the
-- new occurrence column.
CREATE TABLE IF NOT EXISTS `cron_singleton_slots` (
    `singleton_key` varchar(128) NOT NULL,
    `slot` bigint NOT NULL,
    `occurrence_id` varchar(64) NOT NULL,
    `holder` varchar(64) NOT NULL,
    `fencing_token` bigint NOT NULL,
    `lease_until` datetime NOT NULL,
    `updated_at` datetime NOT NULL DEFAULT CURRENT_TIMESTAMP,
    PRIMARY KEY (`singleton_key`, `slot`)
);

CREATE INDEX idx_cron_singleton_slots_lease_until
    ON cron_singleton_slots (lease_until);

CREATE TRIGGER trg_cron_singleton_slots_updated_at
    BEFORE UPDATE ON cron_singleton_slots
    FOR EACH ROW
    SET NEW.updated_at = CURRENT_TIMESTAMP;

ALTER TABLE `cron_occurrences` ADD COLUMN `singleton_slot` bigint NULL;
