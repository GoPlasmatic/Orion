-- R12: per-caller scoping for async trace reads. The 202 returns an opaque
-- capability token; only its SHA-256 hash is stored. NULL for sync traces,
-- DLQ-retry traces and rows written before this migration (those stay on
-- the admin-plane trust model).
ALTER TABLE traces ADD COLUMN IF NOT EXISTS access_token_hash text;
