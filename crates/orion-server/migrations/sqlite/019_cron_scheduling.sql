-- Cron channels (docs/src/reference/channel-config.md#cron-transport): the
-- durable side of a schedule.
--
-- Three tables, and the split is the design. A schedule is *definition*
-- content and lives in the channel's transport_config, versioned and promoted
-- with it. What lives here is the runtime state that definition produces:
-- where each schedule has got to, what it has produced, and which of those are
-- running right now. None of it is authored, none of it is exported, and none
-- of it participates in the draft/active/archived lifecycle.
--
-- `job_leases` deliberately does NOT back any of this. That table is an
-- acquire-per-tick optimisation for periodic maintenance: it carries no
-- occurrence identity, no completion record, and its lease can expire while
-- the work it gated is still running. That is fine for "roughly one node
-- should run the cleanup pass" and unacceptable for "exactly one node may be
-- running this job right now", so correctness lives in schedule-specific rows
-- with their own identities.
--
-- Every timestamp here is written and compared using the DATABASE clock
-- (helpers::sql_now), following storage/repositories/cluster.rs: node clock
-- skew must never decide who owns a lease.

-- One mutable cursor per logical channel: where this schedule has got to.
--
-- Keyed by `channel_id`, not by (channel_id, version): the cursor is the
-- schedule's position in time and survives a new channel version, which is the
-- whole point of `config_hash`. Activating a version whose scheduling fields
-- are unchanged keeps the cursor; changing the expression or the zone resets
-- it to the activation moment rather than retroactively inventing occurrences
-- back to the channel's creation date.
CREATE TABLE IF NOT EXISTS "cron_schedule_state" (
    "channel_id" text NOT NULL PRIMARY KEY,
    -- The version this cursor was last reconciled from. Diagnostic: the cursor
    -- follows the channel, not the version.
    "channel_version" integer NOT NULL,
    -- SHA-256 over the scheduling fields alone — expression, zone, misfire
    -- policy, catch-up bound. Payload and concurrency are excluded so editing
    -- what a job does never silently resets when it next runs.
    "config_hash" text NOT NULL,
    -- The next UTC instant to materialise. Inclusive: the instant it names has
    -- not been produced yet.
    "next_fire_at" timestamp NOT NULL,
    -- Set when the channel leaves the active set (archived, deleted, or
    -- quarantined). This is what tells a later reactivation apart from an
    -- ordinary reload: a paused cursor resumes from the reactivation moment
    -- rather than filling in the gap it was away for.
    "paused_at" timestamp,
    "updated_at" timestamp NOT NULL DEFAULT CURRENT_TIMESTAMP
);

-- The durable run ledger and the work queue, in one table.
--
-- One row per (channel, scheduled instant), created before anything runs and
-- kept after everything has. It is the answer to "did last night's job run?",
-- which is a question no amount of optional trace storage can be trusted to
-- answer: traces are observability and may be sampled, filtered or switched
-- off, while this is scheduling correctness state and is always written.
CREATE TABLE IF NOT EXISTS "cron_occurrences" (
    -- UUID v7: time-ordered, so the primary key index and the listing order
    -- agree and a page of recent occurrences is a range scan.
    "id" text NOT NULL PRIMARY KEY,
    "channel_id" text NOT NULL,
    -- The channel name as it was when this was materialised — an immutable
    -- snapshot, exactly like `traces.channel`. Renaming a channel must not
    -- rewrite the history of what already ran.
    "channel_name" text NOT NULL,
    -- The version that materialised this occurrence…
    "channel_version" integer NOT NULL,
    -- …and the version that actually claimed and ran it, which may differ: a
    -- pending occurrence follows the active generation at claim time, matching
    -- the async queue's contract. Recording both is what makes that visible
    -- rather than surprising. NULL until claimed.
    "executing_version" integer,
    -- Diagnostic snapshot of the binding at materialisation.
    "workflow_id" text,
    -- 'cron' for a scheduled occurrence, 'manual' for one created by
    -- POST /admin/channels/{id}/trigger. A manual occurrence goes through the
    -- identical claim, singleton and execution path; only its provenance and
    -- its scheduled_for differ.
    "trigger" text NOT NULL DEFAULT 'cron',
    -- The immutable UTC instant this occurrence is *for*. With channel_id it
    -- is the occurrence's identity, which is what makes materialisation
    -- idempotent under concurrent reconcilers and across a crash.
    "scheduled_for" timestamp NOT NULL,
    -- pending | claimed | running | completed | failed
    --         | skipped_misfire | skipped_singleton
    "status" text NOT NULL DEFAULT 'pending',
    -- Execution attempts so far. A retry keeps the occurrence identity and
    -- increments this, so `scheduled_for` never lies about when the work was
    -- due.
    "attempt" integer NOT NULL DEFAULT 0,
    "claimed_by" text,
    "claimed_until" timestamp,
    -- The singleton this attempt holds, and the acquisition generation it holds
    -- it under. Both are copied from cron_singletons at acquisition so every
    -- conditional update (heartbeat, settle, release) can be checked against
    -- the row itself rather than against a second lookup.
    "singleton_key" text,
    "fencing_token" integer,
    -- The trace this attempt wrote. NULL until admission.
    "trace_id" text,
    -- Sanitised terminal reason. Never a raw connector error.
    "error_message" text,
    "started_at" timestamp,
    "completed_at" timestamp,
    "created_at" timestamp NOT NULL DEFAULT CURRENT_TIMESTAMP,
    "updated_at" timestamp NOT NULL DEFAULT CURRENT_TIMESTAMP
);

-- The identity, and the reason materialisation can be a plain
-- insert-if-absent: a retried reconciliation tick, or two reconcilers racing,
-- cannot produce two rows for one scheduled instant.
CREATE UNIQUE INDEX IF NOT EXISTS "idx_cron_occurrences_identity"
    ON "cron_occurrences" ("channel_id", "scheduled_for");
-- The claim query: due rows, oldest first.
CREATE INDEX IF NOT EXISTS "idx_cron_occurrences_due"
    ON "cron_occurrences" ("status", "scheduled_for");
-- Expired-claim recovery.
CREATE INDEX IF NOT EXISTS "idx_cron_occurrences_claimed_until"
    ON "cron_occurrences" ("claimed_until");
-- The admin listing's keyset order, matching the traces table's pattern.
CREATE INDEX IF NOT EXISTS "idx_cron_occurrences_created_at_id"
    ON "cron_occurrences" ("created_at", "id");
-- The per-channel status view the status endpoint reads.
CREATE INDEX IF NOT EXISTS "idx_cron_occurrences_channel_created"
    ON "cron_occurrences" ("channel_id", "created_at");

-- One row per *held* singleton key. Absence means the key is free.
--
-- Acquiring this row and moving the occurrence to `running` happen in one
-- transaction, which is the whole non-overlap guarantee: there is no window in
-- which an occurrence is running without holding its key, or holds a key
-- without being recorded as running.
CREATE TABLE IF NOT EXISTS "cron_singletons" (
    "singleton_key" text NOT NULL PRIMARY KEY,
    "occurrence_id" text NOT NULL,
    -- The instance id holding it.
    "holder" text NOT NULL,
    -- Incremented on every acquisition, including a takeover of an expired
    -- lease. Persisted on the occurrence and logged, so a conditional update
    -- from a superseded holder matches nothing.
    --
    -- Diagnostic and future-facing rather than an end-to-end guarantee: Orion's
    -- connectors do not accept a fencing token, so a cancelled holder's
    -- in-flight side effect cannot be rejected downstream by it. What it does
    -- buy is that no *Orion* state is written by a holder that has been
    -- superseded.
    "fencing_token" integer NOT NULL,
    "lease_until" timestamp NOT NULL,
    "updated_at" timestamp NOT NULL DEFAULT CURRENT_TIMESTAMP
);

-- Expired-lease sweeps and the contention view.
CREATE INDEX IF NOT EXISTS "idx_cron_singletons_lease_until"
    ON "cron_singletons" ("lease_until");

CREATE TRIGGER trg_cron_schedule_state_updated_at
AFTER UPDATE ON cron_schedule_state
BEGIN
  UPDATE cron_schedule_state SET updated_at = datetime('now')
  WHERE channel_id = NEW.channel_id;
END;

CREATE TRIGGER trg_cron_occurrences_updated_at
AFTER UPDATE ON cron_occurrences
BEGIN
  UPDATE cron_occurrences SET updated_at = datetime('now')
  WHERE id = NEW.id;
END;

CREATE TRIGGER trg_cron_singletons_updated_at
AFTER UPDATE ON cron_singletons
BEGIN
  UPDATE cron_singletons SET updated_at = datetime('now')
  WHERE singleton_key = NEW.singleton_key;
END;
