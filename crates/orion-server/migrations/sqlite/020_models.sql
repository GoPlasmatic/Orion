-- Models: ONNX models a workflow task can run, stored like every other
-- definition so cluster resync, promotion, backup and audit need no second
-- mechanism.
--
-- One table. Unlike `plugins`, a model version never carries its bytes: the
-- artifact lives in an object-storage bucket and the row holds a *reference*
-- to it — `artifact_json` is the connector, the key, the digest the author
-- claimed and, when known, the size. `digest` repeats the claimed digest as
-- a column so it can be indexed and so a node can confirm it against the
-- bytes it fetches. `models` follows the workflow lifecycle exactly: integer
-- versions, one draft per id, active rows immutable, draft -> active ->
-- archived. Its content columns are the manifest (as validated JSON), the
-- artifact reference, the tags and the optional detached signature over the
-- digest — the same trust arrangement `plugins.signature` has.
--
-- Two columns are *derived*, not authored, and are deliberately outside the
-- active-immutability rule. `admission_json` is a node's verdict on the
-- artifact: `{"state":"pending"}` until a node has fetched it, confirmed the
-- digest and loaded it, then `passed` or `failed` with where and why.
-- `stats_json` is what that probe read out of the model (parameter and node
-- counts, opset, runtime, device) and stays NULL until admission passes.
-- Admission re-runs on a draft only, but the trigger keys on active rows, so
-- leaving these two out is what lets a later `set_admission` / `set_stats`
-- on an active row succeed if it is ever needed — a runtime upgrade that
-- re-probes what it already serves, say — without minting a new version.
-- `schema_parity.rs` lists both under `MUTABLE_WHILE_ACTIVE` for that reason.
--
-- No `current_models` view: the repository reads the latest version per id
-- through `versioned::is_current_version`, as `plugins` does.

CREATE TABLE IF NOT EXISTS "models" (
    "model_id" text NOT NULL,
    "version" integer NOT NULL,
    "status" text NOT NULL DEFAULT 'draft',
    "digest" text NOT NULL,
    "manifest_json" text NOT NULL,
    "artifact_json" text NOT NULL,
    "admission_json" text NOT NULL DEFAULT '{"state":"pending"}',
    "stats_json" text NULL,
    "tags_json" text NOT NULL DEFAULT '[]',
    "signature" text NULL,
    "created_at" timestamp NOT NULL DEFAULT CURRENT_TIMESTAMP,
    "updated_at" timestamp NOT NULL DEFAULT CURRENT_TIMESTAMP,
    PRIMARY KEY ("model_id", "version")
);

CREATE INDEX IF NOT EXISTS idx_models_status ON models(status);
CREATE INDEX IF NOT EXISTS idx_models_digest ON models(digest);

-- Auto-update updated_at
CREATE TRIGGER trg_models_updated_at AFTER UPDATE ON models
BEGIN
  UPDATE models SET updated_at = datetime('now')
  WHERE model_id = NEW.model_id AND version = NEW.version;
END;

-- Only one draft per model_id, on INSERT and on UPDATE alike.
CREATE TRIGGER trg_models_single_draft
BEFORE INSERT ON models
WHEN NEW.status = 'draft'
BEGIN
  SELECT RAISE(ABORT, 'Only one draft version allowed per model')
  WHERE EXISTS (
    SELECT 1 FROM models
    WHERE model_id = NEW.model_id AND status = 'draft'
  );
END;

CREATE TRIGGER trg_models_single_draft_update
BEFORE UPDATE ON models
WHEN NEW.status = 'draft'
BEGIN
  SELECT RAISE(ABORT, 'Only one draft version allowed per model')
  WHERE EXISTS (
    SELECT 1 FROM models
    WHERE model_id = NEW.model_id
      AND status = 'draft'
      AND version <> NEW.version
  );
END;

-- Active rows are immutable: every content column is compared. `IS NOT`
-- rather than `!=` for the nullable signature, so two NULLs compare equal.
-- `admission_json` and `stats_json` are absent on purpose — see the header.
CREATE TRIGGER trg_models_active_immutable
BEFORE UPDATE ON models
WHEN OLD.status = 'active'
  AND NEW.status = 'active'
  AND (OLD.digest != NEW.digest
    OR OLD.manifest_json != NEW.manifest_json
    OR OLD.artifact_json != NEW.artifact_json
    OR OLD.tags_json != NEW.tags_json
    OR OLD.signature IS NOT NEW.signature)
BEGIN
  SELECT RAISE(ABORT, 'Cannot modify content of active models');
END;
