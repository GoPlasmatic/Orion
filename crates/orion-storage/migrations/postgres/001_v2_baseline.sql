-- The v2 baseline: the whole schema of an Orion 2 state database.
--
-- Orion 2 starts from a fresh database. Definitions, connections and
-- deployments live in the immutable deployment store; the 1.x entity tables
-- (workflows, channels, connectors, plugins, models, packages) are gone, and a
-- 1.x database is refused at startup rather than upgraded. The SQLite baseline
-- carries the notes on what each column means; the three schemas are one
-- schema, which tests/schema_parity.rs holds them to.

CREATE FUNCTION update_updated_at_column() RETURNS trigger
    LANGUAGE plpgsql
    AS $$
BEGIN
    NEW.updated_at = CURRENT_TIMESTAMP;
    RETURN NEW;
END;
$$;

-- ============================================================
-- Traces and the trace dead-letter queue
-- ============================================================

CREATE TABLE traces (
    id text NOT NULL,
    channel text DEFAULT 'default'::text NOT NULL,
    channel_id text,
    mode text DEFAULT 'sync'::text NOT NULL,
    status text DEFAULT 'pending'::text NOT NULL,
    input_json text,
    result_json text,
    error_message text,
    duration_ms double precision,
    started_at timestamp without time zone,
    completed_at timestamp without time zone,
    created_at timestamp without time zone DEFAULT CURRENT_TIMESTAMP NOT NULL,
    updated_at timestamp without time zone DEFAULT CURRENT_TIMESTAMP NOT NULL,
    task_trace_json text,
    access_token_hash text,
    PRIMARY KEY (id)
);

CREATE INDEX idx_traces_channel ON traces (channel);

CREATE INDEX idx_traces_channel_id ON traces (channel_id);

CREATE INDEX idx_traces_created_at_id ON traces (created_at, id);

CREATE INDEX idx_traces_mode_created_at ON traces (mode, created_at);

CREATE INDEX idx_traces_status_channel_mode ON traces (status, channel, mode, created_at);

CREATE INDEX idx_traces_updated_at ON traces (updated_at);

CREATE TRIGGER trg_traces_updated_at
    BEFORE UPDATE ON traces
    FOR EACH ROW EXECUTE FUNCTION update_updated_at_column();

CREATE TABLE trace_dlq (
    id text NOT NULL,
    trace_id text NOT NULL,
    channel text NOT NULL,
    payload_json text NOT NULL,
    metadata_json text DEFAULT '{}'::text NOT NULL,
    error_message text NOT NULL,
    retry_count bigint DEFAULT 0 NOT NULL,
    max_retries bigint DEFAULT 5 NOT NULL,
    next_retry_at timestamp without time zone DEFAULT CURRENT_TIMESTAMP NOT NULL,
    created_at timestamp without time zone DEFAULT CURRENT_TIMESTAMP NOT NULL,
    updated_at timestamp without time zone DEFAULT CURRENT_TIMESTAMP NOT NULL,
    claimed_by text,
    claimed_until timestamp without time zone,
    PRIMARY KEY (id)
);

CREATE INDEX idx_trace_dlq_channel_created_at ON trace_dlq (channel, created_at);

CREATE INDEX idx_trace_dlq_claimed_until ON trace_dlq (claimed_until);

CREATE INDEX idx_trace_dlq_created_at ON trace_dlq (created_at);

CREATE INDEX idx_trace_dlq_next_retry ON trace_dlq (next_retry_at) WHERE (retry_count < max_retries);

CREATE TRIGGER trg_trace_dlq_updated_at
    BEFORE UPDATE ON trace_dlq
    FOR EACH ROW EXECUTE FUNCTION update_updated_at_column();

-- ============================================================
-- Audit log
-- ============================================================

CREATE TABLE audit_logs (
    id text NOT NULL,
    principal text NOT NULL,
    action text NOT NULL,
    resource_type text NOT NULL,
    resource_id text NOT NULL,
    details text,
    created_at timestamp without time zone DEFAULT CURRENT_TIMESTAMP NOT NULL,
    PRIMARY KEY (id)
);

CREATE INDEX idx_audit_logs_action_created_at ON audit_logs (action, created_at);

CREATE INDEX idx_audit_logs_created_at ON audit_logs (created_at);

CREATE INDEX idx_audit_logs_principal_created_at ON audit_logs (principal, created_at);

CREATE INDEX idx_audit_logs_resource ON audit_logs (resource_type, resource_id);

-- ============================================================
-- Cron: schedule cursors, the occurrence ledger, singleton slots
-- ============================================================

CREATE TABLE cron_schedule_state (
    channel_id text NOT NULL,
    channel_version bigint NOT NULL,
    config_hash text NOT NULL,
    next_fire_at timestamp without time zone NOT NULL,
    paused_at timestamp without time zone,
    updated_at timestamp without time zone DEFAULT CURRENT_TIMESTAMP NOT NULL,
    PRIMARY KEY (channel_id)
);

CREATE TRIGGER trg_cron_schedule_state_updated_at
    BEFORE UPDATE ON cron_schedule_state
    FOR EACH ROW EXECUTE FUNCTION update_updated_at_column();

CREATE TABLE cron_occurrences (
    id text NOT NULL,
    channel_id text NOT NULL,
    channel_name text NOT NULL,
    channel_version bigint NOT NULL,
    executing_version bigint,
    workflow_id text,
    trigger text DEFAULT 'cron'::text NOT NULL,
    scheduled_for timestamp without time zone NOT NULL,
    status text DEFAULT 'pending'::text NOT NULL,
    attempt bigint DEFAULT 0 NOT NULL,
    claimed_by text,
    claimed_until timestamp without time zone,
    singleton_key text,
    fencing_token bigint,
    trace_id text,
    error_message text,
    started_at timestamp without time zone,
    completed_at timestamp without time zone,
    created_at timestamp without time zone DEFAULT CURRENT_TIMESTAMP NOT NULL,
    updated_at timestamp without time zone DEFAULT CURRENT_TIMESTAMP NOT NULL,
    singleton_slot bigint,
    PRIMARY KEY (id)
);

CREATE INDEX idx_cron_occurrences_channel_created ON cron_occurrences (channel_id, created_at);

CREATE INDEX idx_cron_occurrences_claimed_until ON cron_occurrences (claimed_until);

CREATE INDEX idx_cron_occurrences_created_at_id ON cron_occurrences (created_at, id);

CREATE INDEX idx_cron_occurrences_due ON cron_occurrences (status, scheduled_for);

CREATE UNIQUE INDEX idx_cron_occurrences_identity ON cron_occurrences (channel_id, scheduled_for);

CREATE TRIGGER trg_cron_occurrences_updated_at
    BEFORE UPDATE ON cron_occurrences
    FOR EACH ROW EXECUTE FUNCTION update_updated_at_column();

CREATE TABLE cron_singletons (
    singleton_key text NOT NULL,
    occurrence_id text NOT NULL,
    holder text NOT NULL,
    fencing_token bigint NOT NULL,
    lease_until timestamp without time zone NOT NULL,
    updated_at timestamp without time zone DEFAULT CURRENT_TIMESTAMP NOT NULL,
    PRIMARY KEY (singleton_key)
);

CREATE INDEX idx_cron_singletons_lease_until ON cron_singletons (lease_until);

CREATE TRIGGER trg_cron_singletons_updated_at
    BEFORE UPDATE ON cron_singletons
    FOR EACH ROW EXECUTE FUNCTION update_updated_at_column();

CREATE TABLE cron_singleton_slots (
    singleton_key text NOT NULL,
    slot bigint NOT NULL,
    occurrence_id text NOT NULL,
    holder text NOT NULL,
    fencing_token bigint NOT NULL,
    lease_until timestamp without time zone NOT NULL,
    updated_at timestamp without time zone DEFAULT CURRENT_TIMESTAMP NOT NULL,
    PRIMARY KEY (singleton_key, slot)
);

CREATE INDEX idx_cron_singleton_slots_lease_until ON cron_singleton_slots (lease_until);

CREATE TRIGGER trg_cron_singleton_slots_updated_at
    BEFORE UPDATE ON cron_singleton_slots
    FOR EACH ROW EXECUTE FUNCTION update_updated_at_column();

-- ============================================================
-- Cluster coordination and shared state
-- ============================================================

CREATE TABLE config_epoch (
    id integer NOT NULL,
    breaker_epoch bigint DEFAULT 0 NOT NULL,
    breaker_key text DEFAULT ''::text NOT NULL,
    updated_at timestamp without time zone DEFAULT CURRENT_TIMESTAMP NOT NULL,
    PRIMARY KEY (id),
    CONSTRAINT config_epoch_id_check CHECK ((id = 1))
);

INSERT INTO config_epoch (id) VALUES (1);

CREATE TABLE job_leases (
    job_name text NOT NULL,
    holder text NOT NULL,
    expires_at timestamp without time zone NOT NULL,
    PRIMARY KEY (job_name)
);

CREATE TABLE connector_oauth_state (
    connector_name text NOT NULL,
    fingerprint text NOT NULL,
    state_json text NOT NULL,
    updated_at timestamp without time zone DEFAULT CURRENT_TIMESTAMP NOT NULL,
    PRIMARY KEY (connector_name)
);

CREATE TABLE cluster_kv (
    key_hash character(64) NOT NULL,
    value text NOT NULL,
    expires_at timestamp without time zone,
    PRIMARY KEY (key_hash)
);

CREATE INDEX idx_cluster_kv_expires_at ON cluster_kv (expires_at);

CREATE TABLE cluster_rate_windows (
    limiter_key character(64) NOT NULL,
    window_start bigint NOT NULL,
    hits bigint NOT NULL,
    PRIMARY KEY (limiter_key, window_start)
);

-- ============================================================
-- The immutable deployment store
-- ============================================================

CREATE TABLE artifact_objects (
    digest text NOT NULL,
    size_bytes bigint NOT NULL,
    state text DEFAULT 'active'::text NOT NULL,
    drain_epoch bigint,
    draining_since timestamp without time zone,
    payload_bytes bytea,
    created_at timestamp without time zone DEFAULT CURRENT_TIMESTAMP NOT NULL,
    PRIMARY KEY (digest)
);

CREATE INDEX idx_artifact_objects_state ON artifact_objects (state);

CREATE TABLE provider_packages (
    digest text NOT NULL,
    publisher text NOT NULL,
    provider_id text NOT NULL,
    version text NOT NULL,
    implementation text NOT NULL,
    tier text NOT NULL,
    manifest_json text NOT NULL,
    inventory_json text NOT NULL,
    created_at timestamp without time zone DEFAULT CURRENT_TIMESTAMP NOT NULL,
    PRIMARY KEY (digest)
);

CREATE INDEX idx_provider_packages_identity ON provider_packages (publisher, provider_id, version);

CREATE TABLE provider_admissions (
    package_digest text NOT NULL,
    policy_id text NOT NULL,
    admission_fingerprint text NOT NULL,
    revoked boolean DEFAULT false NOT NULL,
    revoked_reason text,
    admitted_at timestamp without time zone DEFAULT CURRENT_TIMESTAMP NOT NULL,
    updated_at timestamp without time zone DEFAULT CURRENT_TIMESTAMP NOT NULL,
    PRIMARY KEY (package_digest)
);

CREATE TABLE connection_revisions (
    connection_id text NOT NULL,
    revision bigint NOT NULL,
    provider_digest text NOT NULL,
    public_config_json text NOT NULL,
    secret_locators_json text,
    created_at timestamp without time zone DEFAULT CURRENT_TIMESTAMP NOT NULL,
    PRIMARY KEY (connection_id, revision)
);

CREATE TABLE deployment_revisions (
    revision bigint NOT NULL,
    parent_revision bigint,
    manifest_digest text NOT NULL,
    manifest_json text NOT NULL,
    principal text NOT NULL,
    reason text NOT NULL,
    members_compacted boolean DEFAULT false NOT NULL,
    created_at timestamp without time zone DEFAULT CURRENT_TIMESTAMP NOT NULL,
    PRIMARY KEY (revision)
);

CREATE TABLE deployment_members (
    deployment_revision bigint NOT NULL,
    entity_kind text NOT NULL,
    entity_id text NOT NULL,
    entity_revision bigint NOT NULL,
    owning_package text NOT NULL,
    definition_digest text NOT NULL,
    PRIMARY KEY (deployment_revision, entity_kind, entity_id, entity_revision)
);

CREATE TABLE deployment_head (
    singleton_id bigint NOT NULL,
    head_revision bigint NOT NULL,
    epoch bigint NOT NULL,
    updated_at timestamp without time zone DEFAULT CURRENT_TIMESTAMP NOT NULL,
    PRIMARY KEY (singleton_id)
);

CREATE TABLE deployment_operations (
    principal text NOT NULL,
    idempotency_key text NOT NULL,
    request_hash text NOT NULL,
    committed_revision bigint NOT NULL,
    manifest_digest text NOT NULL,
    created_at timestamp without time zone DEFAULT CURRENT_TIMESTAMP NOT NULL,
    operation_id text,
    epoch bigint,
    receiving_node_id text,
    PRIMARY KEY (principal, idempotency_key)
);

CREATE TABLE runtime_nodes (
    node_id text NOT NULL,
    boot_id text NOT NULL,
    desired_revision bigint NOT NULL,
    applied_revision bigint NOT NULL,
    applied_manifest_digest text,
    configuration_degraded boolean DEFAULT false NOT NULL,
    retained_revisions_json text NOT NULL,
    security_lease_until timestamp without time zone NOT NULL,
    updated_at timestamp without time zone DEFAULT CURRENT_TIMESTAMP NOT NULL,
    PRIMARY KEY (node_id)
);

CREATE INDEX idx_runtime_nodes_security_lease ON runtime_nodes (security_lease_until);

-- ============================================================
-- Source channels: partition claims and checkpoints
-- ============================================================

CREATE TABLE source_claims (
    channel_id text NOT NULL,
    partition_id text NOT NULL,
    owner_id text NOT NULL,
    revision bigint NOT NULL,
    checkpoint_namespace text NOT NULL,
    fence_token bigint NOT NULL,
    lease_until timestamp without time zone NOT NULL,
    updated_at timestamp without time zone DEFAULT CURRENT_TIMESTAMP NOT NULL,
    PRIMARY KEY (channel_id, partition_id)
);

CREATE INDEX idx_source_claims_lease_until ON source_claims (lease_until);

CREATE TABLE source_checkpoints (
    channel_id text NOT NULL,
    partition_id text NOT NULL,
    checkpoint_namespace text NOT NULL,
    fence_token bigint NOT NULL,
    contiguous_offset bigint NOT NULL,
    cursor_json text NOT NULL,
    updated_at timestamp without time zone DEFAULT CURRENT_TIMESTAMP NOT NULL,
    PRIMARY KEY (channel_id, partition_id, checkpoint_namespace)
);
