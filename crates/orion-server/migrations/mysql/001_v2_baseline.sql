-- The v2 baseline: the whole schema of an Orion 2 state database.
--
-- Orion 2 starts from a fresh database. Definitions, connections and
-- deployments live in the immutable deployment store; the 1.x entity tables
-- (workflows, channels, connectors, plugins, models, packages) are gone, and a
-- 1.x database is refused at startup rather than upgraded. The SQLite baseline
-- carries the notes on what each column means; the three schemas are one
-- schema, which tests/schema_parity.rs holds them to.

-- ============================================================
-- Traces and the trace dead-letter queue
-- ============================================================

CREATE TABLE `traces` (
  `id` varchar(36) NOT NULL,
  `channel` varchar(255) NOT NULL DEFAULT 'default',
  `channel_id` varchar(255) DEFAULT NULL,
  `mode` varchar(16) NOT NULL DEFAULT 'sync',
  `status` varchar(32) NOT NULL DEFAULT 'pending',
  `input_json` text,
  `result_json` text,
  `error_message` text,
  `duration_ms` double DEFAULT NULL,
  `started_at` datetime DEFAULT NULL,
  `completed_at` datetime DEFAULT NULL,
  `created_at` datetime NOT NULL DEFAULT CURRENT_TIMESTAMP,
  `updated_at` datetime NOT NULL DEFAULT CURRENT_TIMESTAMP,
  `task_trace_json` text,
  `access_token_hash` char(64) DEFAULT NULL,
  PRIMARY KEY (`id`),
  KEY `idx_traces_channel` (`channel`),
  KEY `idx_traces_channel_id` (`channel_id`),
  KEY `idx_traces_status_channel_mode` (`status`,`channel`,`mode`,`created_at`),
  KEY `idx_traces_mode_created_at` (`mode`,`created_at`),
  KEY `idx_traces_updated_at` (`updated_at`),
  KEY `idx_traces_created_at_id` (`created_at`,`id`)
) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4;

CREATE TRIGGER trg_traces_updated_at
    BEFORE UPDATE ON traces
    FOR EACH ROW
    SET NEW.updated_at = CURRENT_TIMESTAMP;

CREATE TABLE `trace_dlq` (
  `id` varchar(36) NOT NULL,
  `trace_id` varchar(36) NOT NULL,
  `channel` varchar(255) NOT NULL,
  `payload_json` text NOT NULL,
  `metadata_json` text NOT NULL,
  `error_message` text NOT NULL,
  `retry_count` bigint NOT NULL DEFAULT '0',
  `max_retries` bigint NOT NULL DEFAULT '5',
  `next_retry_at` datetime NOT NULL DEFAULT CURRENT_TIMESTAMP,
  `created_at` datetime NOT NULL DEFAULT CURRENT_TIMESTAMP,
  `updated_at` datetime NOT NULL DEFAULT CURRENT_TIMESTAMP,
  `claimed_by` varchar(64) DEFAULT NULL,
  `claimed_until` datetime DEFAULT NULL,
  PRIMARY KEY (`id`),
  KEY `idx_trace_dlq_next_retry` (`next_retry_at`,`retry_count`),
  KEY `idx_trace_dlq_claimed_until` (`claimed_until`),
  KEY `idx_trace_dlq_created_at` (`created_at`),
  KEY `idx_trace_dlq_channel_created_at` (`channel`,`created_at`)
) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4;

CREATE TRIGGER trg_trace_dlq_updated_at
    BEFORE UPDATE ON trace_dlq
    FOR EACH ROW
    SET NEW.updated_at = CURRENT_TIMESTAMP;

-- ============================================================
-- Audit log
-- ============================================================

CREATE TABLE `audit_logs` (
  `id` varchar(36) NOT NULL,
  `principal` varchar(255) NOT NULL,
  `action` varchar(64) NOT NULL,
  `resource_type` varchar(64) NOT NULL,
  `resource_id` varchar(255) NOT NULL,
  `details` text,
  `created_at` datetime NOT NULL DEFAULT CURRENT_TIMESTAMP,
  PRIMARY KEY (`id`),
  KEY `idx_audit_logs_created_at` (`created_at`),
  KEY `idx_audit_logs_resource` (`resource_type`,`resource_id`),
  KEY `idx_audit_logs_principal_created_at` (`principal`,`created_at`),
  KEY `idx_audit_logs_action_created_at` (`action`,`created_at`)
) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4;

-- ============================================================
-- Cron: schedule cursors, the occurrence ledger, singleton slots
-- ============================================================

CREATE TABLE `cron_schedule_state` (
  `channel_id` varchar(255) NOT NULL,
  `channel_version` bigint NOT NULL,
  `config_hash` varchar(80) NOT NULL,
  `next_fire_at` datetime NOT NULL,
  `paused_at` datetime DEFAULT NULL,
  `updated_at` datetime NOT NULL DEFAULT CURRENT_TIMESTAMP,
  PRIMARY KEY (`channel_id`)
) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4;

CREATE TRIGGER trg_cron_schedule_state_updated_at
    BEFORE UPDATE ON cron_schedule_state
    FOR EACH ROW
    SET NEW.updated_at = CURRENT_TIMESTAMP;

CREATE TABLE `cron_occurrences` (
  `id` varchar(64) NOT NULL,
  `channel_id` varchar(255) NOT NULL,
  `channel_name` varchar(255) NOT NULL,
  `channel_version` bigint NOT NULL,
  `executing_version` bigint DEFAULT NULL,
  `workflow_id` varchar(255) DEFAULT NULL,
  `trigger` varchar(16) NOT NULL DEFAULT 'cron',
  `scheduled_for` datetime NOT NULL,
  `status` varchar(24) NOT NULL DEFAULT 'pending',
  `attempt` bigint NOT NULL DEFAULT '0',
  `claimed_by` varchar(64) DEFAULT NULL,
  `claimed_until` datetime DEFAULT NULL,
  `singleton_key` varchar(128) DEFAULT NULL,
  `fencing_token` bigint DEFAULT NULL,
  `trace_id` varchar(64) DEFAULT NULL,
  `error_message` text,
  `started_at` datetime DEFAULT NULL,
  `completed_at` datetime DEFAULT NULL,
  `created_at` datetime NOT NULL DEFAULT CURRENT_TIMESTAMP,
  `updated_at` datetime NOT NULL DEFAULT CURRENT_TIMESTAMP,
  `singleton_slot` bigint DEFAULT NULL,
  PRIMARY KEY (`id`),
  UNIQUE KEY `idx_cron_occurrences_identity` (`channel_id`,`scheduled_for`),
  KEY `idx_cron_occurrences_due` (`status`,`scheduled_for`),
  KEY `idx_cron_occurrences_claimed_until` (`claimed_until`),
  KEY `idx_cron_occurrences_created_at_id` (`created_at`,`id`),
  KEY `idx_cron_occurrences_channel_created` (`channel_id`,`created_at`)
) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4;

CREATE TRIGGER trg_cron_occurrences_updated_at
    BEFORE UPDATE ON cron_occurrences
    FOR EACH ROW
    SET NEW.updated_at = CURRENT_TIMESTAMP;

CREATE TABLE `cron_singletons` (
  `singleton_key` varchar(128) NOT NULL,
  `occurrence_id` varchar(64) NOT NULL,
  `holder` varchar(64) NOT NULL,
  `fencing_token` bigint NOT NULL,
  `lease_until` datetime NOT NULL,
  `updated_at` datetime NOT NULL DEFAULT CURRENT_TIMESTAMP,
  PRIMARY KEY (`singleton_key`),
  KEY `idx_cron_singletons_lease_until` (`lease_until`)
) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4;

CREATE TRIGGER trg_cron_singletons_updated_at
    BEFORE UPDATE ON cron_singletons
    FOR EACH ROW
    SET NEW.updated_at = CURRENT_TIMESTAMP;

CREATE TABLE `cron_singleton_slots` (
  `singleton_key` varchar(128) NOT NULL,
  `slot` bigint NOT NULL,
  `occurrence_id` varchar(64) NOT NULL,
  `holder` varchar(64) NOT NULL,
  `fencing_token` bigint NOT NULL,
  `lease_until` datetime NOT NULL,
  `updated_at` datetime NOT NULL DEFAULT CURRENT_TIMESTAMP,
  PRIMARY KEY (`singleton_key`,`slot`),
  KEY `idx_cron_singleton_slots_lease_until` (`lease_until`)
) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4;

CREATE TRIGGER trg_cron_singleton_slots_updated_at
    BEFORE UPDATE ON cron_singleton_slots
    FOR EACH ROW
    SET NEW.updated_at = CURRENT_TIMESTAMP;

-- ============================================================
-- Cluster coordination and shared state
-- ============================================================

CREATE TABLE `config_epoch` (
  `id` int NOT NULL,
  `breaker_epoch` bigint NOT NULL DEFAULT '0',
  `breaker_key` varchar(255) NOT NULL DEFAULT '',
  `updated_at` datetime NOT NULL DEFAULT CURRENT_TIMESTAMP,
  PRIMARY KEY (`id`),
  CONSTRAINT `config_epoch_chk_1` CHECK ((`id` = 1))
) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4;

INSERT INTO `config_epoch` (`id`) VALUES (1);

CREATE TABLE `job_leases` (
  `job_name` varchar(128) NOT NULL,
  `holder` varchar(64) NOT NULL,
  `expires_at` datetime NOT NULL,
  PRIMARY KEY (`job_name`)
) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4;

CREATE TABLE `connector_oauth_state` (
  `connector_name` varchar(255) NOT NULL,
  `fingerprint` varchar(64) NOT NULL,
  `state_json` text NOT NULL,
  `updated_at` datetime NOT NULL DEFAULT CURRENT_TIMESTAMP,
  PRIMARY KEY (`connector_name`)
) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4;

CREATE TABLE `cluster_kv` (
  `key_hash` char(64) NOT NULL,
  `value` longtext NOT NULL,
  `expires_at` datetime DEFAULT NULL,
  PRIMARY KEY (`key_hash`),
  KEY `idx_cluster_kv_expires_at` (`expires_at`)
) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4;

CREATE TABLE `cluster_rate_windows` (
  `limiter_key` char(64) NOT NULL,
  `window_start` bigint NOT NULL,
  `hits` bigint NOT NULL,
  PRIMARY KEY (`limiter_key`,`window_start`)
) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4;

-- ============================================================
-- The immutable deployment store
-- ============================================================

CREATE TABLE `artifact_objects` (
  `digest` varchar(128) NOT NULL,
  `size_bytes` bigint NOT NULL,
  `state` varchar(32) NOT NULL DEFAULT 'active',
  `drain_epoch` bigint DEFAULT NULL,
  `draining_since` datetime DEFAULT NULL,
  `payload_bytes` longblob,
  `created_at` datetime NOT NULL DEFAULT CURRENT_TIMESTAMP,
  PRIMARY KEY (`digest`),
  KEY `idx_artifact_objects_state` (`state`)
) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4;

CREATE TABLE `provider_packages` (
  `digest` varchar(128) NOT NULL,
  `publisher` varchar(128) NOT NULL,
  `provider_id` varchar(128) NOT NULL,
  `version` varchar(64) NOT NULL,
  `implementation` varchar(32) NOT NULL,
  `tier` varchar(32) NOT NULL,
  `manifest_json` text NOT NULL,
  `inventory_json` text NOT NULL,
  `created_at` datetime NOT NULL DEFAULT CURRENT_TIMESTAMP,
  PRIMARY KEY (`digest`),
  KEY `idx_provider_packages_identity` (`publisher`,`provider_id`,`version`)
) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4;

CREATE TABLE `provider_admissions` (
  `package_digest` varchar(128) NOT NULL,
  `policy_id` varchar(128) NOT NULL,
  `admission_fingerprint` varchar(128) NOT NULL,
  `revoked` tinyint(1) NOT NULL DEFAULT '0',
  `revoked_reason` text,
  `admitted_at` datetime NOT NULL DEFAULT CURRENT_TIMESTAMP,
  `updated_at` datetime NOT NULL DEFAULT CURRENT_TIMESTAMP,
  PRIMARY KEY (`package_digest`)
) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4;

CREATE TABLE `connection_revisions` (
  `connection_id` varchar(128) NOT NULL,
  `revision` bigint NOT NULL,
  `provider_digest` varchar(128) NOT NULL,
  `public_config_json` text NOT NULL,
  `secret_locators_json` text,
  `created_at` datetime NOT NULL DEFAULT CURRENT_TIMESTAMP,
  PRIMARY KEY (`connection_id`,`revision`)
) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4;

CREATE TABLE `deployment_revisions` (
  `revision` bigint NOT NULL,
  `parent_revision` bigint DEFAULT NULL,
  `manifest_digest` varchar(128) NOT NULL,
  `manifest_json` text NOT NULL,
  `principal` varchar(128) NOT NULL,
  `reason` text NOT NULL,
  `members_compacted` tinyint(1) NOT NULL DEFAULT '0',
  `created_at` datetime NOT NULL DEFAULT CURRENT_TIMESTAMP,
  PRIMARY KEY (`revision`)
) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4;

CREATE TABLE `deployment_members` (
  `deployment_revision` bigint NOT NULL,
  `entity_kind` varchar(32) NOT NULL,
  `entity_id` varchar(128) NOT NULL,
  `entity_revision` bigint NOT NULL,
  `owning_package` varchar(128) NOT NULL,
  `definition_digest` varchar(128) NOT NULL,
  PRIMARY KEY (`deployment_revision`,`entity_kind`,`entity_id`,`entity_revision`)
) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4;

CREATE TABLE `deployment_head` (
  `singleton_id` bigint NOT NULL,
  `head_revision` bigint NOT NULL,
  `epoch` bigint NOT NULL,
  `updated_at` datetime NOT NULL DEFAULT CURRENT_TIMESTAMP,
  PRIMARY KEY (`singleton_id`)
) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4;

CREATE TABLE `deployment_operations` (
  `principal` varchar(128) NOT NULL,
  `idempotency_key` varchar(128) NOT NULL,
  `request_hash` varchar(128) NOT NULL,
  `committed_revision` bigint NOT NULL,
  `manifest_digest` varchar(128) NOT NULL,
  `created_at` datetime NOT NULL DEFAULT CURRENT_TIMESTAMP,
  `operation_id` varchar(128) DEFAULT NULL,
  `epoch` bigint DEFAULT NULL,
  `receiving_node_id` varchar(255) DEFAULT NULL,
  PRIMARY KEY (`principal`,`idempotency_key`)
) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4;

CREATE TABLE `runtime_nodes` (
  `node_id` varchar(128) NOT NULL,
  `boot_id` varchar(128) NOT NULL,
  `desired_revision` bigint NOT NULL,
  `applied_revision` bigint NOT NULL,
  `applied_manifest_digest` varchar(128) DEFAULT NULL,
  `configuration_degraded` tinyint(1) NOT NULL DEFAULT '0',
  `retained_revisions_json` text NOT NULL,
  `security_lease_until` datetime NOT NULL,
  `updated_at` datetime NOT NULL DEFAULT CURRENT_TIMESTAMP,
  PRIMARY KEY (`node_id`),
  KEY `idx_runtime_nodes_security_lease` (`security_lease_until`)
) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4;

-- ============================================================
-- Source channels: partition claims and checkpoints
-- ============================================================

CREATE TABLE `source_claims` (
  `channel_id` varchar(128) NOT NULL,
  `partition_id` varchar(128) NOT NULL,
  `owner_id` varchar(128) NOT NULL,
  `revision` bigint NOT NULL,
  `checkpoint_namespace` varchar(128) NOT NULL,
  `fence_token` bigint NOT NULL,
  `lease_until` datetime NOT NULL,
  `updated_at` datetime NOT NULL DEFAULT CURRENT_TIMESTAMP,
  PRIMARY KEY (`channel_id`,`partition_id`),
  KEY `idx_source_claims_lease_until` (`lease_until`)
) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4;

CREATE TABLE `source_checkpoints` (
  `channel_id` varchar(128) NOT NULL,
  `partition_id` varchar(128) NOT NULL,
  `checkpoint_namespace` varchar(128) NOT NULL,
  `fence_token` bigint NOT NULL,
  `contiguous_offset` bigint NOT NULL,
  `cursor_json` text NOT NULL,
  `updated_at` datetime NOT NULL DEFAULT CURRENT_TIMESTAMP,
  PRIMARY KEY (`channel_id`,`partition_id`,`checkpoint_namespace`)
) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4;
