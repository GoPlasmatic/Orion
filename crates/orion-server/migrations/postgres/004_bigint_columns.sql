-- Widen integer columns to bigint (multi-instance-ha 0.8).
-- The Rust models decode these as i64; sqlx-postgres rejects reading INT4
-- into i64 ("mismatched types ... INT8 is not compatible with INT4"), so
-- every repository read failed on Postgres. Ships as an ALTER (not a 001
-- edit) because 001 applies cleanly and is checksum-frozen.
-- The views must be dropped first: postgres refuses to alter a column's
-- type while a view depends on it. Recreated identically below.

DROP VIEW current_workflows;
DROP VIEW current_channels;

ALTER TABLE workflows
    ALTER COLUMN version TYPE bigint,
    ALTER COLUMN priority TYPE bigint,
    ALTER COLUMN rollout_percentage TYPE bigint;

ALTER TABLE channels
    ALTER COLUMN version TYPE bigint,
    ALTER COLUMN priority TYPE bigint;

ALTER TABLE trace_dlq
    ALTER COLUMN retry_count TYPE bigint,
    ALTER COLUMN max_retries TYPE bigint;

-- Latest version per workflow_id
CREATE VIEW current_workflows AS
SELECT w.*
FROM workflows w
INNER JOIN (
  SELECT workflow_id, MAX(version) AS max_version
  FROM workflows
  GROUP BY workflow_id
) latest ON w.workflow_id = latest.workflow_id AND w.version = latest.max_version;

-- Latest version per channel_id
CREATE VIEW current_channels AS
SELECT c.*
FROM channels c
INNER JOIN (
  SELECT channel_id, MAX(version) AS max_version
  FROM channels
  GROUP BY channel_id
) latest ON c.channel_id = latest.channel_id AND c.version = latest.max_version;
