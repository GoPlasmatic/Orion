-- V2-03.2..V2-03.4: Immutable deployment storage, artifact objects, provider
-- packages/admissions, connection revisions, deployment head/revisions/members/
-- operations, source revisions/claims/checkpoints, and runtime nodes.

CREATE TABLE IF NOT EXISTS `artifact_objects` (
    `digest` varchar(128) NOT NULL,
    `size_bytes` bigint NOT NULL,
    `state` varchar(32) NOT NULL DEFAULT 'active',
    `drain_epoch` bigint NULL,
    `draining_since` datetime NULL,
    `payload_bytes` longblob NULL,
    `created_at` datetime NOT NULL DEFAULT CURRENT_TIMESTAMP,
    PRIMARY KEY (`digest`)
);

CREATE INDEX idx_artifact_objects_state
    ON artifact_objects (state);

CREATE TABLE IF NOT EXISTS `provider_packages` (
    `digest` varchar(128) NOT NULL,
    `publisher` varchar(128) NOT NULL,
    `provider_id` varchar(128) NOT NULL,
    `version` varchar(64) NOT NULL,
    `implementation` varchar(32) NOT NULL,
    `tier` varchar(32) NOT NULL,
    `manifest_json` text NOT NULL,
    `inventory_json` text NOT NULL,
    `created_at` datetime NOT NULL DEFAULT CURRENT_TIMESTAMP,
    PRIMARY KEY (`digest`)
);

CREATE INDEX idx_provider_packages_identity
    ON provider_packages (publisher, provider_id, version);

CREATE TABLE IF NOT EXISTS `provider_admissions` (
    `package_digest` varchar(128) NOT NULL,
    `policy_id` varchar(128) NOT NULL,
    `admission_fingerprint` varchar(128) NOT NULL,
    `revoked` tinyint(1) NOT NULL DEFAULT 0,
    `revoked_reason` text NULL,
    `admitted_at` datetime NOT NULL DEFAULT CURRENT_TIMESTAMP,
    `updated_at` datetime NOT NULL DEFAULT CURRENT_TIMESTAMP,
    PRIMARY KEY (`package_digest`)
);

CREATE TABLE IF NOT EXISTS `connection_revisions` (
    `connection_id` varchar(128) NOT NULL,
    `revision` bigint NOT NULL,
    `provider_digest` varchar(128) NOT NULL,
    `public_config_json` text NOT NULL,
    `secret_locator` text NULL,
    `created_at` datetime NOT NULL DEFAULT CURRENT_TIMESTAMP,
    PRIMARY KEY (`connection_id`, `revision`)
);

CREATE TABLE IF NOT EXISTS `deployment_head` (
    `singleton_id` bigint NOT NULL,
    `head_revision` bigint NOT NULL,
    `epoch` bigint NOT NULL,
    `updated_at` datetime NOT NULL DEFAULT CURRENT_TIMESTAMP,
    PRIMARY KEY (`singleton_id`)
);

CREATE TABLE IF NOT EXISTS `deployment_revisions` (
    `revision` bigint NOT NULL,
    `parent_revision` bigint NULL,
    `manifest_digest` varchar(128) NOT NULL,
    `manifest_json` text NOT NULL,
    `principal` varchar(128) NOT NULL,
    `reason` text NOT NULL,
    `members_compacted` tinyint(1) NOT NULL DEFAULT 0,
    `created_at` datetime NOT NULL DEFAULT CURRENT_TIMESTAMP,
    PRIMARY KEY (`revision`)
);

CREATE TABLE IF NOT EXISTS `deployment_members` (
    `deployment_revision` bigint NOT NULL,
    `entity_kind` varchar(32) NOT NULL,
    `entity_id` varchar(128) NOT NULL,
    `entity_revision` bigint NOT NULL,
    `owning_package` varchar(128) NOT NULL,
    `definition_digest` varchar(128) NOT NULL,
    PRIMARY KEY (`deployment_revision`, `entity_kind`, `entity_id`, `entity_revision`)
);

CREATE TABLE IF NOT EXISTS `deployment_operations` (
    `principal` varchar(128) NOT NULL,
    `idempotency_key` varchar(128) NOT NULL,
    `request_hash` varchar(128) NOT NULL,
    `committed_revision` bigint NOT NULL,
    `manifest_digest` varchar(128) NOT NULL,
    `created_at` datetime NOT NULL DEFAULT CURRENT_TIMESTAMP,
    PRIMARY KEY (`principal`, `idempotency_key`)
);

CREATE TABLE IF NOT EXISTS `source_revisions` (
    `channel_id` varchar(128) NOT NULL,
    `revision` bigint NOT NULL,
    `connection_id` varchar(128) NOT NULL,
    `connection_revision` bigint NOT NULL,
    `checkpoint_namespace` varchar(128) NOT NULL,
    `config_json` text NOT NULL,
    `created_at` datetime NOT NULL DEFAULT CURRENT_TIMESTAMP,
    PRIMARY KEY (`channel_id`, `revision`)
);

CREATE TABLE IF NOT EXISTS `source_claims` (
    `channel_id` varchar(128) NOT NULL,
    `partition_id` varchar(128) NOT NULL,
    `owner_id` varchar(128) NOT NULL,
    `revision` bigint NOT NULL,
    `checkpoint_namespace` varchar(128) NOT NULL,
    `fence_token` bigint NOT NULL,
    `lease_until` datetime NOT NULL,
    `updated_at` datetime NOT NULL DEFAULT CURRENT_TIMESTAMP,
    PRIMARY KEY (`channel_id`, `partition_id`)
);

CREATE INDEX idx_source_claims_lease_until
    ON source_claims (lease_until);

CREATE TABLE IF NOT EXISTS `source_checkpoints` (
    `channel_id` varchar(128) NOT NULL,
    `partition_id` varchar(128) NOT NULL,
    `checkpoint_namespace` varchar(128) NOT NULL,
    `fence_token` bigint NOT NULL,
    `contiguous_offset` bigint NOT NULL,
    `cursor_json` text NOT NULL,
    `updated_at` datetime NOT NULL DEFAULT CURRENT_TIMESTAMP,
    PRIMARY KEY (`channel_id`, `partition_id`, `checkpoint_namespace`)
);

CREATE TABLE IF NOT EXISTS `runtime_nodes` (
    `node_id` varchar(128) NOT NULL,
    `boot_id` varchar(128) NOT NULL,
    `desired_revision` bigint NOT NULL,
    `applied_revision` bigint NOT NULL,
    `applied_manifest_digest` varchar(128) NULL,
    `configuration_degraded` tinyint(1) NOT NULL DEFAULT 0,
    `retained_revisions_json` text NOT NULL,
    `security_lease_until` datetime NOT NULL,
    `updated_at` datetime NOT NULL DEFAULT CURRENT_TIMESTAMP,
    PRIMARY KEY (`node_id`)
);

CREATE INDEX idx_runtime_nodes_security_lease
    ON runtime_nodes (security_lease_until);
