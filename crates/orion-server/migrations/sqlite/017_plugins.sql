-- Plugins (plugin.md): custom task functions as sandboxed WebAssembly
-- Components, stored like every other definition so cluster resync,
-- promotion, backup and audit need no second mechanism.
--
-- Two tables. `plugin_artifacts` holds component bytes keyed by their
-- SHA-256 digest — immutable and deduplicated: a version names a digest,
-- and however many versions name the same one, the bytes are stored once.
-- `plugins` is the versioned entity and follows the workflow lifecycle
-- exactly: integer versions, one draft per id, active rows immutable,
-- draft -> active -> archived. Its content columns are the manifest (as the
-- validated JSON form, not the TOML text), the digest and the tags.
--
-- No `current_plugins` view: the repositories read the latest version per
-- id through `versioned::is_current_version`, which is what the two older
-- views were retained for one release to bridge.

CREATE TABLE IF NOT EXISTS "plugin_artifacts" (
    "digest" text NOT NULL PRIMARY KEY,
    "bytes" blob NOT NULL,
    "size" integer NOT NULL,
    "created_at" timestamp NOT NULL DEFAULT CURRENT_TIMESTAMP
);

CREATE TABLE IF NOT EXISTS "plugins" (
    "plugin_id" text NOT NULL,
    "version" integer NOT NULL,
    "status" text NOT NULL DEFAULT 'draft',
    "digest" text NOT NULL,
    "manifest_json" text NOT NULL,
    "tags_json" text NOT NULL DEFAULT '[]',
    "created_at" timestamp NOT NULL DEFAULT CURRENT_TIMESTAMP,
    "updated_at" timestamp NOT NULL DEFAULT CURRENT_TIMESTAMP,
    PRIMARY KEY ("plugin_id", "version")
);

CREATE INDEX IF NOT EXISTS idx_plugins_status ON plugins(status);
CREATE INDEX IF NOT EXISTS idx_plugins_digest ON plugins(digest);

-- Auto-update updated_at
CREATE TRIGGER trg_plugins_updated_at AFTER UPDATE ON plugins
BEGIN
  UPDATE plugins SET updated_at = datetime('now')
  WHERE plugin_id = NEW.plugin_id AND version = NEW.version;
END;

-- Only one draft per plugin_id, on INSERT and on UPDATE alike.
CREATE TRIGGER trg_plugins_single_draft
BEFORE INSERT ON plugins
WHEN NEW.status = 'draft'
BEGIN
  SELECT RAISE(ABORT, 'Only one draft version allowed per plugin')
  WHERE EXISTS (
    SELECT 1 FROM plugins
    WHERE plugin_id = NEW.plugin_id AND status = 'draft'
  );
END;

CREATE TRIGGER trg_plugins_single_draft_update
BEFORE UPDATE ON plugins
WHEN NEW.status = 'draft'
BEGIN
  SELECT RAISE(ABORT, 'Only one draft version allowed per plugin')
  WHERE EXISTS (
    SELECT 1 FROM plugins
    WHERE plugin_id = NEW.plugin_id
      AND status = 'draft'
      AND version <> NEW.version
  );
END;

-- Active rows are immutable: every content column is compared.
CREATE TRIGGER trg_plugins_active_immutable
BEFORE UPDATE ON plugins
WHEN OLD.status = 'active'
  AND NEW.status = 'active'
  AND (OLD.digest != NEW.digest
    OR OLD.manifest_json != NEW.manifest_json
    OR OLD.tags_json != NEW.tags_json)
BEGIN
  SELECT RAISE(ABORT, 'Cannot modify content of active plugins');
END;
