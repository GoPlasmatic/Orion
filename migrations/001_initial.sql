-- ============================================================
-- Rules table (versioned, composite PK)
-- ============================================================
CREATE TABLE IF NOT EXISTS rules (
    rule_id TEXT NOT NULL,
    version INTEGER NOT NULL,
    name TEXT NOT NULL,
    description TEXT,
    channel TEXT NOT NULL DEFAULT 'default',
    priority INTEGER NOT NULL DEFAULT 0,
    status TEXT NOT NULL DEFAULT 'draft' CHECK (status IN ('draft', 'active', 'archived')),
    rollout_percentage INTEGER NOT NULL DEFAULT 100 CHECK (rollout_percentage BETWEEN 0 AND 100),
    condition_json TEXT NOT NULL DEFAULT 'true',
    tasks_json TEXT NOT NULL DEFAULT '[]',
    tags TEXT NOT NULL DEFAULT '[]',
    continue_on_error INTEGER NOT NULL DEFAULT 0,
    created_at TEXT NOT NULL DEFAULT (datetime('now')),
    updated_at TEXT NOT NULL DEFAULT (datetime('now')),
    PRIMARY KEY (rule_id, version)
);

CREATE INDEX idx_rules_status ON rules(status);
CREATE INDEX idx_rules_channel_status ON rules(channel, status);
CREATE INDEX idx_rules_priority_name ON rules(priority DESC, name ASC);
CREATE INDEX idx_rules_rule_id ON rules(rule_id);

-- updated_at trigger
CREATE TRIGGER trg_rules_updated_at AFTER UPDATE ON rules
BEGIN
  UPDATE rules SET updated_at = datetime('now')
  WHERE rule_id = NEW.rule_id AND version = NEW.version;
END;

-- Only one draft per rule_id
CREATE TRIGGER trg_rules_single_draft
BEFORE INSERT ON rules
WHEN NEW.status = 'draft'
BEGIN
  SELECT RAISE(ABORT, 'Only one draft version allowed per rule')
  WHERE EXISTS (
    SELECT 1 FROM rules
    WHERE rule_id = NEW.rule_id AND status = 'draft'
  );
END;

-- Prevent content changes on active rules
CREATE TRIGGER trg_rules_active_immutable
BEFORE UPDATE ON rules
WHEN OLD.status = 'active'
  AND NEW.status = 'active'
  AND (OLD.name != NEW.name
    OR OLD.description IS NOT NEW.description
    OR OLD.channel != NEW.channel
    OR OLD.priority != NEW.priority
    OR OLD.condition_json != NEW.condition_json
    OR OLD.tasks_json != NEW.tasks_json
    OR OLD.tags != NEW.tags
    OR OLD.continue_on_error != NEW.continue_on_error)
BEGIN
  SELECT RAISE(ABORT, 'Cannot modify content of active rules');
END;

-- Latest version per rule_id
CREATE VIEW current_rules AS
SELECT r.*
FROM rules r
INNER JOIN (
  SELECT rule_id, MAX(version) AS max_version
  FROM rules
  GROUP BY rule_id
) latest ON r.rule_id = latest.rule_id AND r.version = latest.max_version;

-- ============================================================
-- Connectors table
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
CREATE INDEX idx_traces_mode ON traces(mode);

CREATE TRIGGER trg_traces_updated_at AFTER UPDATE ON traces
BEGIN
  UPDATE traces SET updated_at = datetime('now') WHERE id = NEW.id;
END;
