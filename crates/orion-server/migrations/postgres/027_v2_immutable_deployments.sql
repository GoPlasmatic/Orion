-- V2-03.2..V2-03.4: Immutable deployment storage, artifact objects, provider
-- packages/admissions, connection revisions, deployment head/revisions/members/
-- operations, source revisions/claims/checkpoints, and runtime nodes.

CREATE TABLE IF NOT EXISTS "artifact_objects" (
    "digest" text NOT NULL,
    "size_bytes" bigint NOT NULL,
    "state" text NOT NULL DEFAULT 'active',
    "drain_epoch" bigint,
    "draining_since" timestamp,
    "payload_bytes" bytea,
    "created_at" timestamp NOT NULL DEFAULT CURRENT_TIMESTAMP,
    PRIMARY KEY ("digest")
);

CREATE INDEX IF NOT EXISTS "idx_artifact_objects_state"
    ON "artifact_objects" ("state");

CREATE TABLE IF NOT EXISTS "provider_packages" (
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

CREATE INDEX IF NOT EXISTS "idx_provider_packages_identity"
    ON "provider_packages" ("publisher", "provider_id", "version");

CREATE TABLE IF NOT EXISTS "provider_admissions" (
    "package_digest" text NOT NULL,
    "policy_id" text NOT NULL,
    "admission_fingerprint" text NOT NULL,
    "revoked" boolean NOT NULL DEFAULT FALSE,
    "revoked_reason" text,
    "admitted_at" timestamp NOT NULL DEFAULT CURRENT_TIMESTAMP,
    "updated_at" timestamp NOT NULL DEFAULT CURRENT_TIMESTAMP,
    PRIMARY KEY ("package_digest")
);

CREATE TABLE IF NOT EXISTS "connection_revisions" (
    "connection_id" text NOT NULL,
    "revision" bigint NOT NULL,
    "provider_digest" text NOT NULL,
    "public_config_json" text NOT NULL,
    "secret_locator" text,
    "created_at" timestamp NOT NULL DEFAULT CURRENT_TIMESTAMP,
    PRIMARY KEY ("connection_id", "revision")
);

CREATE TABLE IF NOT EXISTS "deployment_head" (
    "singleton_id" bigint NOT NULL,
    "head_revision" bigint NOT NULL,
    "epoch" bigint NOT NULL,
    "updated_at" timestamp NOT NULL DEFAULT CURRENT_TIMESTAMP,
    PRIMARY KEY ("singleton_id")
);

CREATE TABLE IF NOT EXISTS "deployment_revisions" (
    "revision" bigint NOT NULL,
    "parent_revision" bigint,
    "manifest_digest" text NOT NULL,
    "manifest_json" text NOT NULL,
    "principal" text NOT NULL,
    "reason" text NOT NULL,
    "members_compacted" boolean NOT NULL DEFAULT FALSE,
    "created_at" timestamp NOT NULL DEFAULT CURRENT_TIMESTAMP,
    PRIMARY KEY ("revision")
);

CREATE TABLE IF NOT EXISTS "deployment_members" (
    "deployment_revision" bigint NOT NULL,
    "entity_kind" text NOT NULL,
    "entity_id" text NOT NULL,
    "entity_revision" bigint NOT NULL,
    "owning_package" text NOT NULL,
    "definition_digest" text NOT NULL,
    PRIMARY KEY ("deployment_revision", "entity_kind", "entity_id", "entity_revision")
);

CREATE TABLE IF NOT EXISTS "deployment_operations" (
    "principal" text NOT NULL,
    "idempotency_key" text NOT NULL,
    "request_hash" text NOT NULL,
    "committed_revision" bigint NOT NULL,
    "manifest_digest" text NOT NULL,
    "created_at" timestamp NOT NULL DEFAULT CURRENT_TIMESTAMP,
    PRIMARY KEY ("principal", "idempotency_key")
);

CREATE TABLE IF NOT EXISTS "source_revisions" (
    "channel_id" text NOT NULL,
    "revision" bigint NOT NULL,
    "connection_id" text NOT NULL,
    "connection_revision" bigint NOT NULL,
    "checkpoint_namespace" text NOT NULL,
    "config_json" text NOT NULL,
    "created_at" timestamp NOT NULL DEFAULT CURRENT_TIMESTAMP,
    PRIMARY KEY ("channel_id", "revision")
);

CREATE TABLE IF NOT EXISTS "source_claims" (
    "channel_id" text NOT NULL,
    "partition_id" text NOT NULL,
    "owner_id" text NOT NULL,
    "revision" bigint NOT NULL,
    "checkpoint_namespace" text NOT NULL,
    "fence_token" bigint NOT NULL,
    "lease_until" timestamp NOT NULL,
    "updated_at" timestamp NOT NULL DEFAULT CURRENT_TIMESTAMP,
    PRIMARY KEY ("channel_id", "partition_id")
);

CREATE INDEX IF NOT EXISTS "idx_source_claims_lease_until"
    ON "source_claims" ("lease_until");

CREATE TABLE IF NOT EXISTS "source_checkpoints" (
    "channel_id" text NOT NULL,
    "partition_id" text NOT NULL,
    "checkpoint_namespace" text NOT NULL,
    "fence_token" bigint NOT NULL,
    "contiguous_offset" bigint NOT NULL,
    "cursor_json" text NOT NULL,
    "updated_at" timestamp NOT NULL DEFAULT CURRENT_TIMESTAMP,
    PRIMARY KEY ("channel_id", "partition_id", "checkpoint_namespace")
);

CREATE TABLE IF NOT EXISTS "runtime_nodes" (
    "node_id" text NOT NULL,
    "boot_id" text NOT NULL,
    "desired_revision" bigint NOT NULL,
    "applied_revision" bigint NOT NULL,
    "applied_manifest_digest" text,
    "configuration_degraded" boolean NOT NULL DEFAULT FALSE,
    "retained_revisions_json" text NOT NULL,
    "security_lease_until" timestamp NOT NULL,
    "updated_at" timestamp NOT NULL DEFAULT CURRENT_TIMESTAMP,
    PRIMARY KEY ("node_id")
);

CREATE INDEX IF NOT EXISTS "idx_runtime_nodes_security_lease"
    ON "runtime_nodes" ("security_lease_until");
