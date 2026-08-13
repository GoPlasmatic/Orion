-- Self-heal the two "current version" views (proposal C6).
--
-- 004_bigint_columns.sql drops both views and recreates them, but the DROPs
-- have no IF EXISTS and the whole thing runs as one table-rewriting migration
-- under ACCESS EXCLUSIVE. If the connection drops midway, the database is left
-- with 004 *recorded as applied* but without `current_workflows` /
-- `current_channels` — and sqlx will never re-run version 004. Every
-- `list_paginated` then fails, and nothing recovers automatically.
--
-- 004 is checksum-frozen and cannot be edited, so this migration restores the
-- invariant instead. CREATE OR REPLACE VIEW is idempotent: a no-op when 004
-- completed, a repair when it did not.
--
-- The definitions below must stay byte-identical in *shape* to the ones in
-- 004; they are the same SELECTs.

CREATE OR REPLACE VIEW current_workflows AS
SELECT w.*
FROM workflows w
INNER JOIN (
  SELECT workflow_id, MAX(version) AS max_version
  FROM workflows
  GROUP BY workflow_id
) latest ON w.workflow_id = latest.workflow_id AND w.version = latest.max_version;

CREATE OR REPLACE VIEW current_channels AS
SELECT c.*
FROM channels c
INNER JOIN (
  SELECT channel_id, MAX(version) AS max_version
  FROM channels
  GROUP BY channel_id
) latest ON c.channel_id = latest.channel_id AND c.version = latest.max_version;
