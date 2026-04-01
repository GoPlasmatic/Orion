-- ============================================================
-- Workflows table (versioned, composite PK) — replaces rules
-- ============================================================
CREATE TABLE IF NOT EXISTS workflows (
    workflow_id TEXT NOT NULL,
    version INTEGER NOT NULL,
    name TEXT NOT NULL,
    description TEXT,
    priority INTEGER NOT NULL DEFAULT 0,
    status TEXT NOT NULL DEFAULT 'draft' CHECK (status IN ('draft', 'active', 'archived')),
    rollout_percentage INTEGER NOT NULL DEFAULT 100 CHECK (rollout_percentage BETWEEN 0 AND 100),
    condition_json TEXT NOT NULL DEFAULT 'true',
    tasks_json TEXT NOT NULL DEFAULT '[]',
    tags TEXT NOT NULL DEFAULT '[]',
    continue_on_error INTEGER NOT NULL DEFAULT 0,
    created_at TEXT NOT NULL DEFAULT (datetime('now')),
    updated_at TEXT NOT NULL DEFAULT (datetime('now')),
    PRIMARY KEY (workflow_id, version)
);

CREATE INDEX idx_workflows_status ON workflows(status);
CREATE INDEX idx_workflows_workflow_id ON workflows(workflow_id);
CREATE INDEX idx_workflows_priority_name ON workflows(priority DESC, name ASC);

-- updated_at trigger
CREATE TRIGGER trg_workflows_updated_at AFTER UPDATE ON workflows
BEGIN
  UPDATE workflows SET updated_at = datetime('now')
  WHERE workflow_id = NEW.workflow_id AND version = NEW.version;
END;

-- Only one draft per workflow_id
CREATE TRIGGER trg_workflows_single_draft
BEFORE INSERT ON workflows
WHEN NEW.status = 'draft'
BEGIN
  SELECT RAISE(ABORT, 'Only one draft version allowed per workflow')
  WHERE EXISTS (
    SELECT 1 FROM workflows
    WHERE workflow_id = NEW.workflow_id AND status = 'draft'
  );
END;

-- Prevent content changes on active workflows
CREATE TRIGGER trg_workflows_active_immutable
BEFORE UPDATE ON workflows
WHEN OLD.status = 'active'
  AND NEW.status = 'active'
  AND (OLD.name != NEW.name
    OR OLD.description IS NOT NEW.description
    OR OLD.priority != NEW.priority
    OR OLD.condition_json != NEW.condition_json
    OR OLD.tasks_json != NEW.tasks_json
    OR OLD.tags != NEW.tags
    OR OLD.continue_on_error != NEW.continue_on_error)
BEGIN
  SELECT RAISE(ABORT, 'Cannot modify content of active workflows');
END;

-- Latest version per workflow_id
CREATE VIEW current_workflows AS
SELECT w.*
FROM workflows w
INNER JOIN (
  SELECT workflow_id, MAX(version) AS max_version
  FROM workflows
  GROUP BY workflow_id
) latest ON w.workflow_id = latest.workflow_id AND w.version = latest.max_version;

-- ============================================================
-- Channels table (versioned, composite PK)
-- ============================================================
CREATE TABLE IF NOT EXISTS channels (
    channel_id TEXT NOT NULL,
    version INTEGER NOT NULL,
    name TEXT NOT NULL,
    description TEXT,
    channel_type TEXT NOT NULL CHECK (channel_type IN ('sync', 'async')),
    protocol TEXT NOT NULL CHECK (protocol IN ('rest', 'http', 'kafka')),
    -- Sync-specific fields (NULL for async channels)
    methods TEXT,                  -- JSON array: ["GET","POST"]
    route_pattern TEXT,            -- e.g. "/orders/{id}"
    -- Async-specific fields (NULL for sync channels)
    topic TEXT,                    -- Kafka topic name
    consumer_group TEXT,           -- Kafka consumer group
    transport_config_json TEXT NOT NULL DEFAULT '{}',
    -- Workflow binding
    workflow_id TEXT,              -- References workflows.workflow_id by convention
    -- Per-channel baseline config (JSON blob)
    config_json TEXT NOT NULL DEFAULT '{}',
    -- Lifecycle
    status TEXT NOT NULL DEFAULT 'draft' CHECK (status IN ('draft', 'active', 'archived')),
    priority INTEGER NOT NULL DEFAULT 0,
    created_at TEXT NOT NULL DEFAULT (datetime('now')),
    updated_at TEXT NOT NULL DEFAULT (datetime('now')),
    PRIMARY KEY (channel_id, version)
);

CREATE INDEX idx_channels_status ON channels(status);
CREATE INDEX idx_channels_name ON channels(name);
CREATE INDEX idx_channels_type_status ON channels(channel_type, status);
CREATE INDEX idx_channels_route ON channels(route_pattern) WHERE route_pattern IS NOT NULL;
CREATE INDEX idx_channels_topic ON channels(topic) WHERE topic IS NOT NULL;
CREATE INDEX idx_channels_workflow ON channels(workflow_id) WHERE workflow_id IS NOT NULL;
CREATE INDEX idx_channels_channel_id ON channels(channel_id);

-- updated_at trigger
CREATE TRIGGER trg_channels_updated_at AFTER UPDATE ON channels
BEGIN
  UPDATE channels SET updated_at = datetime('now')
  WHERE channel_id = NEW.channel_id AND version = NEW.version;
END;

-- Only one draft per channel_id
CREATE TRIGGER trg_channels_single_draft
BEFORE INSERT ON channels
WHEN NEW.status = 'draft'
BEGIN
  SELECT RAISE(ABORT, 'Only one draft version allowed per channel')
  WHERE EXISTS (
    SELECT 1 FROM channels
    WHERE channel_id = NEW.channel_id AND status = 'draft'
  );
END;

-- Prevent content changes on active channels
CREATE TRIGGER trg_channels_active_immutable
BEFORE UPDATE ON channels
WHEN OLD.status = 'active'
  AND NEW.status = 'active'
  AND (OLD.name != NEW.name
    OR OLD.description IS NOT NEW.description
    OR OLD.channel_type != NEW.channel_type
    OR OLD.protocol != NEW.protocol
    OR OLD.methods IS NOT NEW.methods
    OR OLD.route_pattern IS NOT NEW.route_pattern
    OR OLD.topic IS NOT NEW.topic
    OR OLD.consumer_group IS NOT NEW.consumer_group
    OR OLD.workflow_id IS NOT NEW.workflow_id
    OR OLD.config_json != NEW.config_json)
BEGIN
  SELECT RAISE(ABORT, 'Cannot modify content of active channels');
END;

-- Latest version per channel_id
CREATE VIEW current_channels AS
SELECT c.*
FROM channels c
INNER JOIN (
  SELECT channel_id, MAX(version) AS max_version
  FROM channels
  GROUP BY channel_id
) latest ON c.channel_id = latest.channel_id AND c.version = latest.max_version;

-- ============================================================
-- Connectors table (unchanged from original)
-- ============================================================
CREATE TABLE IF NOT EXISTS connectors (
    id TEXT PRIMARY KEY NOT NULL,
    name TEXT NOT NULL UNIQUE,
    connector_type TEXT NOT NULL,
    config_json TEXT NOT NULL DEFAULT '{}',
    enabled INTEGER NOT NULL DEFAULT 1,
    created_at TEXT NOT NULL DEFAULT (datetime('now')),
    updated_at TEXT NOT NULL DEFAULT (datetime('now'))
);

CREATE TRIGGER trg_connectors_updated_at AFTER UPDATE ON connectors
BEGIN
  UPDATE connectors SET updated_at = datetime('now') WHERE id = NEW.id;
END;

-- ============================================================
-- Traces table
-- ============================================================
CREATE TABLE IF NOT EXISTS traces (
    id TEXT PRIMARY KEY NOT NULL,
    channel TEXT NOT NULL DEFAULT 'default',
    channel_id TEXT,
    mode TEXT NOT NULL DEFAULT 'sync' CHECK (mode IN ('sync', 'async')),
    status TEXT NOT NULL DEFAULT 'pending' CHECK (status IN ('pending', 'running', 'completed', 'failed')),
    input_json TEXT,
    result_json TEXT,
    error_message TEXT,
    duration_ms REAL,
    started_at TEXT,
    completed_at TEXT,
    created_at TEXT NOT NULL DEFAULT (datetime('now')),
    updated_at TEXT NOT NULL DEFAULT (datetime('now'))
);

CREATE INDEX idx_traces_status ON traces(status);
CREATE INDEX idx_traces_status_channel ON traces(status, channel);
CREATE INDEX idx_traces_channel ON traces(channel);
CREATE INDEX idx_traces_channel_id ON traces(channel_id) WHERE channel_id IS NOT NULL;
CREATE INDEX idx_traces_mode ON traces(mode);
CREATE INDEX idx_traces_created_at ON traces(created_at);

CREATE TRIGGER trg_traces_updated_at AFTER UPDATE ON traces
BEGIN
  UPDATE traces SET updated_at = datetime('now') WHERE id = NEW.id;
END;
