-- Bounded concurrency for cron channels: `concurrency.slots` lets up to N
-- occurrences of one singleton key run at once. Slot 0 stays the
-- `cron_singletons` row it has always been — an older binary and this one
-- contend for it with the same statements, so a key with one slot behaves
-- exactly as before in every version mix — and slots 1..N-1 live here.
-- Expand-only: an older binary neither reads nor writes this table or the
-- new occurrence column.
CREATE TABLE IF NOT EXISTS "cron_singleton_slots" (
    "singleton_key" text NOT NULL,
    -- Always >= 1: slot 0 is the `cron_singletons` row.
    "slot" integer NOT NULL,
    "occurrence_id" text NOT NULL,
    "holder" text NOT NULL,
    "fencing_token" integer NOT NULL,
    "lease_until" timestamp NOT NULL,
    "updated_at" timestamp NOT NULL DEFAULT CURRENT_TIMESTAMP,
    PRIMARY KEY ("singleton_key", "slot")
);

CREATE INDEX IF NOT EXISTS "idx_cron_singleton_slots_lease_until"
    ON "cron_singleton_slots" ("lease_until");

CREATE TRIGGER trg_cron_singleton_slots_updated_at
AFTER UPDATE ON cron_singleton_slots
BEGIN
  UPDATE cron_singleton_slots SET updated_at = datetime('now')
  WHERE singleton_key = NEW.singleton_key AND slot = NEW.slot;
END;

-- Which slot an occurrence held: two occurrences of one key may now carry
-- the same fencing token, and the slot is what tells them apart.
ALTER TABLE "cron_occurrences" ADD COLUMN "singleton_slot" integer;
