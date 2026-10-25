-- The v2 baseline: the whole schema of an Orion 2 state database.
--
-- Orion 2 starts from a fresh database. Definitions, connections and
-- deployments live in the immutable deployment store; the 1.x entity tables
-- (workflows, channels, connectors, plugins, models, packages) are gone, and a
-- 1.x database is refused at startup rather than upgraded.

-- ============================================================
-- Traces and the trace dead-letter queue
-- ============================================================

CREATE TABLE "traces" (
    "id" text NOT NULL PRIMARY KEY,
    "channel" text NOT NULL DEFAULT 'default',
    "channel_id" text,
    "mode" text NOT NULL DEFAULT 'sync',
    "status" text NOT NULL DEFAULT 'pending',
    "input_json" text,
    "result_json" text,
    "task_trace_json" text,
    "error_message" text,
    "duration_ms" double,
    -- SHA-256 of the per-trace read token an async caller is handed, never the
    -- token itself.
    "access_token_hash" text,
    "started_at" timestamp_text,
    "completed_at" timestamp_text,
    "created_at" timestamp_text NOT NULL DEFAULT CURRENT_TIMESTAMP,
    "updated_at" timestamp_text NOT NULL DEFAULT CURRENT_TIMESTAMP
);

CREATE INDEX "idx_traces_channel" ON "traces" ("channel");
CREATE INDEX "idx_traces_channel_id" ON "traces" ("channel_id");
CREATE INDEX "idx_traces_status_channel_mode" ON "traces" ("status", "channel", "mode", "created_at");
CREATE INDEX "idx_traces_mode_created_at" ON "traces" ("mode", "created_at");
CREATE INDEX "idx_traces_updated_at" ON "traces" ("updated_at");
CREATE INDEX "idx_traces_created_at_id" ON "traces" ("created_at", "id");

CREATE TRIGGER trg_traces_updated_at AFTER UPDATE ON traces
BEGIN
  UPDATE traces SET updated_at = datetime('now') WHERE id = NEW.id;
END;

CREATE TABLE "trace_dlq" (
    "id" text NOT NULL PRIMARY KEY,
    "trace_id" text NOT NULL,
    "channel" text NOT NULL,
    "payload_json" text NOT NULL,
    "metadata_json" text NOT NULL DEFAULT '{}',
    "error_message" text NOT NULL,
    "retry_count" integer NOT NULL DEFAULT 0,
    "max_retries" integer NOT NULL DEFAULT 5,
    "next_retry_at" timestamp_text NOT NULL DEFAULT CURRENT_TIMESTAMP,
    -- The node retrying this entry, and until when. A lapsed lease is free.
    "claimed_by" text,
    "claimed_until" timestamp,
    "created_at" timestamp_text NOT NULL DEFAULT CURRENT_TIMESTAMP,
    "updated_at" timestamp_text NOT NULL DEFAULT CURRENT_TIMESTAMP
);

CREATE INDEX "idx_trace_dlq_next_retry" ON "trace_dlq" ("next_retry_at") WHERE "retry_count" < "max_retries";
CREATE INDEX "idx_trace_dlq_channel_created_at" ON "trace_dlq" ("channel", "created_at");
CREATE INDEX "idx_trace_dlq_claimed_until" ON "trace_dlq" ("claimed_until");
CREATE INDEX "idx_trace_dlq_created_at" ON "trace_dlq" ("created_at");

CREATE TRIGGER trg_trace_dlq_updated_at AFTER UPDATE ON trace_dlq
BEGIN
  UPDATE trace_dlq SET updated_at = datetime('now') WHERE id = NEW.id;
END;

-- ============================================================
-- Audit log
-- ============================================================

CREATE TABLE "audit_logs" (
    "id" text NOT NULL PRIMARY KEY,
    "principal" text NOT NULL,
    "action" text NOT NULL,
    "resource_type" text NOT NULL,
    "resource_id" text NOT NULL,
    "details" text,
    "created_at" timestamp_text NOT NULL DEFAULT CURRENT_TIMESTAMP
);

CREATE INDEX "idx_audit_logs_created_at" ON "audit_logs" ("created_at");
CREATE INDEX "idx_audit_logs_resource" ON "audit_logs" ("resource_type", "resource_id");
CREATE INDEX "idx_audit_logs_principal_created_at" ON "audit_logs" ("principal", "created_at");
CREATE INDEX "idx_audit_logs_action_created_at" ON "audit_logs" ("action", "created_at");

-- ============================================================
-- Cron: schedule cursors, the occurrence ledger, singleton slots
-- ============================================================

CREATE TABLE "cron_schedule_state" (
    "channel_id" text NOT NULL PRIMARY KEY,
    -- The revision this cursor was last reconciled from. Diagnostic: the
    -- cursor follows the channel, not the revision.
    "channel_version" integer NOT NULL,
    -- SHA-256 over the scheduling fields alone — expression, zone, misfire
    -- policy, catch-up bound. Payload and concurrency are excluded so editing
    -- what a job does never silently resets when it next runs.
    "config_hash" text NOT NULL,
    -- The next UTC instant to materialise. Inclusive: the instant it names has
    -- not been produced yet.
    "next_fire_at" timestamp NOT NULL,
    -- Set when the channel leaves the served set (undeployed or quarantined).
    -- This is what tells a later return apart from an ordinary rebuild: a
    -- paused cursor resumes from the moment it returns rather than filling in
    -- the gap it was away for.
    "paused_at" timestamp,
    "updated_at" timestamp NOT NULL DEFAULT CURRENT_TIMESTAMP
);

CREATE TRIGGER trg_cron_schedule_state_updated_at
AFTER UPDATE ON cron_schedule_state
BEGIN
  UPDATE cron_schedule_state SET updated_at = datetime('now')
  WHERE channel_id = NEW.channel_id;
END;

CREATE TABLE "cron_occurrences" (
    -- UUID v7: time-ordered, so the primary key index and the listing order
    -- agree and a page of recent occurrences is a range scan.
    "id" text NOT NULL PRIMARY KEY,
    "channel_id" text NOT NULL,
    -- The channel name as it was when this was materialised — an immutable
    -- snapshot, exactly like `traces.channel`.
    "channel_name" text NOT NULL,
    -- The revision that materialised this occurrence…
    "channel_version" integer NOT NULL,
    -- …and the revision that actually claimed and ran it, which may differ: a
    -- pending occurrence follows the served generation at claim time, matching
    -- the async queue's contract. NULL until claimed.
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
    -- The singleton slot this attempt holds and the acquisition generation it
    -- holds it under, copied at acquisition so every conditional update
    -- (heartbeat, settle, release) can be checked against the row itself.
    "singleton_key" text,
    "singleton_slot" integer,
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

CREATE UNIQUE INDEX "idx_cron_occurrences_identity"
    ON "cron_occurrences" ("channel_id", "scheduled_for");
CREATE INDEX "idx_cron_occurrences_due"
    ON "cron_occurrences" ("status", "scheduled_for");
CREATE INDEX "idx_cron_occurrences_claimed_until"
    ON "cron_occurrences" ("claimed_until");
CREATE INDEX "idx_cron_occurrences_created_at_id"
    ON "cron_occurrences" ("created_at", "id");
CREATE INDEX "idx_cron_occurrences_channel_created"
    ON "cron_occurrences" ("channel_id", "created_at");

CREATE TRIGGER trg_cron_occurrences_updated_at
AFTER UPDATE ON cron_occurrences
BEGIN
  UPDATE cron_occurrences SET updated_at = datetime('now')
  WHERE id = NEW.id;
END;

-- Slot 0 of every `forbid` key.
CREATE TABLE "cron_singletons" (
    "singleton_key" text NOT NULL PRIMARY KEY,
    "occurrence_id" text NOT NULL,
    -- The instance id holding it.
    "holder" text NOT NULL,
    -- Incremented on every acquisition, including a takeover of an expired
    -- lease. Persisted on the occurrence and logged, so a conditional update
    -- from a superseded holder matches nothing. Orion's connectors do not
    -- accept a fencing token, so a cancelled holder's in-flight side effect
    -- cannot be rejected downstream by it; what it does buy is that no *Orion*
    -- state is written by a holder that has been superseded.
    "fencing_token" integer NOT NULL,
    "lease_until" timestamp NOT NULL,
    "updated_at" timestamp NOT NULL DEFAULT CURRENT_TIMESTAMP
);

CREATE INDEX "idx_cron_singletons_lease_until"
    ON "cron_singletons" ("lease_until");

CREATE TRIGGER trg_cron_singletons_updated_at
AFTER UPDATE ON cron_singletons
BEGIN
  UPDATE cron_singletons SET updated_at = datetime('now')
  WHERE singleton_key = NEW.singleton_key;
END;

-- Slots 1 and up of a key with `concurrency.slots` above one.
CREATE TABLE "cron_singleton_slots" (
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

CREATE INDEX "idx_cron_singleton_slots_lease_until"
    ON "cron_singleton_slots" ("lease_until");

CREATE TRIGGER trg_cron_singleton_slots_updated_at
AFTER UPDATE ON cron_singleton_slots
BEGIN
  UPDATE cron_singleton_slots SET updated_at = datetime('now')
  WHERE singleton_key = NEW.singleton_key AND slot = NEW.slot;
END;

-- ============================================================
-- Cluster coordination and shared state
-- ============================================================

-- One row. `breaker_epoch` moves when an operator resets a circuit breaker,
-- and `breaker_key` names which, so every node resets its own copy.
CREATE TABLE "config_epoch" (
    "id" integer NOT NULL PRIMARY KEY CHECK ("id" = 1),
    "breaker_epoch" bigint NOT NULL DEFAULT 0,
    "breaker_key" text NOT NULL DEFAULT '',
    "updated_at" timestamp NOT NULL DEFAULT CURRENT_TIMESTAMP
);

INSERT INTO "config_epoch" ("id") VALUES (1);

CREATE TABLE "job_leases" (
    "job_name" text NOT NULL PRIMARY KEY,
    "holder" text NOT NULL,
    "expires_at" timestamp NOT NULL
);

-- A connector's OAuth2 token state, shared so every node refreshes one token.
CREATE TABLE "connector_oauth_state" (
    "connector_name" text NOT NULL PRIMARY KEY,
    "fingerprint" text NOT NULL,
    "state_json" text NOT NULL,
    "updated_at" timestamp NOT NULL DEFAULT CURRENT_TIMESTAMP
);

-- The cluster's dedup store, response cache and namespace counters, judged on
-- the database's clock.
CREATE TABLE "cluster_kv" (
    "key_hash" text NOT NULL PRIMARY KEY,
    "value" text NOT NULL,
    "expires_at" timestamp
);

CREATE INDEX "idx_cluster_kv_expires_at" ON "cluster_kv" ("expires_at");

-- Fleet-wide fixed rate-limit windows, one row per (limiter key, second).
CREATE TABLE "cluster_rate_windows" (
    "limiter_key" text NOT NULL,
    "window_start" bigint NOT NULL,
    "hits" bigint NOT NULL,
    PRIMARY KEY ("limiter_key", "window_start")
);

-- ============================================================
-- The immutable deployment store
-- ============================================================

-- Content-addressed bytes: definitions and provider package files.
CREATE TABLE "artifact_objects" (
    "digest" text NOT NULL,
    "size_bytes" integer NOT NULL,
    "state" text NOT NULL DEFAULT 'active',
    "drain_epoch" integer,
    "draining_since" timestamp,
    "payload_bytes" blob,
    "created_at" timestamp NOT NULL DEFAULT CURRENT_TIMESTAMP,
    PRIMARY KEY ("digest")
);

CREATE INDEX "idx_artifact_objects_state"
    ON "artifact_objects" ("state");

CREATE TABLE "provider_packages" (
    "digest" text NOT NULL,
    "publisher" text NOT NULL,
    "provider_id" text NOT NULL,
    "version" text NOT NULL,
    "implementation" text NOT NULL,
    "tier" text NOT NULL,
    "manifest_json" text NOT NULL,
    "inventory_json" text NOT NULL,
    "created_at" timestamp NOT NULL DEFAULT CURRENT_TIMESTAMP,
    PRIMARY KEY ("digest")
);

CREATE INDEX "idx_provider_packages_identity"
    ON "provider_packages" ("publisher", "provider_id", "version");

CREATE TABLE "provider_admissions" (
    "package_digest" text NOT NULL,
    "policy_id" text NOT NULL,
    "admission_fingerprint" text NOT NULL,
    "revoked" boolean NOT NULL DEFAULT 0,
    "revoked_reason" text,
    "admitted_at" timestamp NOT NULL DEFAULT CURRENT_TIMESTAMP,
    "updated_at" timestamp NOT NULL DEFAULT CURRENT_TIMESTAMP,
    PRIMARY KEY ("package_digest")
);

-- A connection revision. Secrets are never stored: `secret_locators_json` is
-- the canonical JSON map from a config path to the reference (`env://…`,
-- `vault://…`) resolved at load, or NULL when there are none.
CREATE TABLE "connection_revisions" (
    "connection_id" text NOT NULL,
    "revision" integer NOT NULL,
    "provider_digest" text NOT NULL,
    "public_config_json" text NOT NULL,
    "secret_locators_json" text,
    "created_at" timestamp NOT NULL DEFAULT CURRENT_TIMESTAMP,
    PRIMARY KEY ("connection_id", "revision")
);

CREATE TABLE "deployment_revisions" (
    "revision" integer NOT NULL,
    "parent_revision" integer,
    "manifest_digest" text NOT NULL,
    "manifest_json" text NOT NULL,
    "principal" text NOT NULL,
    "reason" text NOT NULL,
    "members_compacted" boolean NOT NULL DEFAULT 0,
    "created_at" timestamp NOT NULL DEFAULT CURRENT_TIMESTAMP,
    PRIMARY KEY ("revision")
);

CREATE TABLE "deployment_members" (
    "deployment_revision" integer NOT NULL,
    "entity_kind" text NOT NULL,
    "entity_id" text NOT NULL,
    "entity_revision" integer NOT NULL,
    "owning_package" text NOT NULL,
    "definition_digest" text NOT NULL,
    PRIMARY KEY ("deployment_revision", "entity_kind", "entity_id", "entity_revision")
);

-- One row: the committed revision every node converges on. `epoch` is the
-- compare-and-swap token a deployment names.
CREATE TABLE "deployment_head" (
    "singleton_id" integer NOT NULL,
    "head_revision" integer NOT NULL,
    "epoch" integer NOT NULL,
    "updated_at" timestamp NOT NULL DEFAULT CURRENT_TIMESTAMP,
    PRIMARY KEY ("singleton_id")
);

-- Idempotent replay of a deployment request: one row per (principal, key).
CREATE TABLE "deployment_operations" (
    "principal" text NOT NULL,
    "idempotency_key" text NOT NULL,
    "request_hash" text NOT NULL,
    "committed_revision" integer NOT NULL,
    "manifest_digest" text NOT NULL,
    "operation_id" text,
    "epoch" bigint,
    "receiving_node_id" text,
    "created_at" timestamp NOT NULL DEFAULT CURRENT_TIMESTAMP,
    PRIMARY KEY ("principal", "idempotency_key")
);

-- Each node's convergence report.
CREATE TABLE "runtime_nodes" (
    "node_id" text NOT NULL,
    "boot_id" text NOT NULL,
    "desired_revision" integer NOT NULL,
    "applied_revision" integer NOT NULL,
    "applied_manifest_digest" text,
    "configuration_degraded" boolean NOT NULL DEFAULT 0,
    "retained_revisions_json" text NOT NULL,
    "security_lease_until" timestamp NOT NULL,
    "updated_at" timestamp NOT NULL DEFAULT CURRENT_TIMESTAMP,
    PRIMARY KEY ("node_id")
);

CREATE INDEX "idx_runtime_nodes_security_lease"
    ON "runtime_nodes" ("security_lease_until");

-- ============================================================
-- Source channels: partition claims and checkpoints
-- ============================================================

CREATE TABLE "source_claims" (
    "channel_id" text NOT NULL,
    "partition_id" text NOT NULL,
    "owner_id" text NOT NULL,
    "revision" integer NOT NULL,
    "checkpoint_namespace" text NOT NULL,
    "fence_token" integer NOT NULL,
    "lease_until" timestamp NOT NULL,
    "updated_at" timestamp NOT NULL DEFAULT CURRENT_TIMESTAMP,
    PRIMARY KEY ("channel_id", "partition_id")
);

CREATE INDEX "idx_source_claims_lease_until"
    ON "source_claims" ("lease_until");

CREATE TABLE "source_checkpoints" (
    "channel_id" text NOT NULL,
    "partition_id" text NOT NULL,
    "checkpoint_namespace" text NOT NULL,
    "fence_token" integer NOT NULL,
    "contiguous_offset" integer NOT NULL,
    "cursor_json" text NOT NULL,
    "updated_at" timestamp NOT NULL DEFAULT CURRENT_TIMESTAMP,
    PRIMARY KEY ("channel_id", "partition_id", "checkpoint_namespace")
);
