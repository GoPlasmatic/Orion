-- Add result_json column to jobs for storing async processing results
ALTER TABLE jobs ADD COLUMN result_json TEXT;

-- Add channel column to jobs for data API jobs
ALTER TABLE jobs ADD COLUMN channel TEXT;

-- Seed a synthetic connector for data API async jobs
INSERT OR IGNORE INTO connectors (id, name, connector_type, config_json, enabled)
VALUES ('__data_api__', '__data_api__', 'internal', '{}', 0);
