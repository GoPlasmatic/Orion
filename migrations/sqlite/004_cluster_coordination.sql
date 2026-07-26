-- Cluster coordination primitives (multi-instance-ha A2/A3/A7/A11).
-- config_epoch: single-row counter bumped on every admin mutation; each node
-- polls it and resyncs from the DB when it advances. breaker_epoch/breaker_key
-- piggyback circuit-breaker reset fan-out on the same row.
-- job_leases: acquire-per-tick single-flight for background jobs.
-- trace_dlq lease columns: per-row claims so one node retries each entry.

CREATE TABLE IF NOT EXISTS "config_epoch" (
    "id" integer NOT NULL PRIMARY KEY CHECK ("id" = 1),
    "epoch" bigint NOT NULL DEFAULT 0,
    "breaker_epoch" bigint NOT NULL DEFAULT 0,
    "breaker_key" text NOT NULL DEFAULT '',
    "updated_at" timestamp NOT NULL DEFAULT CURRENT_TIMESTAMP
);
INSERT OR IGNORE INTO "config_epoch" ("id") VALUES (1);

CREATE TABLE IF NOT EXISTS "job_leases" (
    "job_name" text NOT NULL PRIMARY KEY,
    "holder" text NOT NULL,
    "expires_at" timestamp NOT NULL
);

ALTER TABLE "trace_dlq" ADD COLUMN "claimed_by" text;
ALTER TABLE "trace_dlq" ADD COLUMN "claimed_until" timestamp;
CREATE INDEX "idx_trace_dlq_claimed_until" ON "trace_dlq" ("claimed_until");
