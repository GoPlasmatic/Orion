-- Package receipt inventory: the entities one package version carried, by
-- conflict key per kind, as JSON ({"plugins": [...], "models": [...],
-- "connectors": [...], "workflows": [...], "channels": [...]}). What
-- `package apply --prune` diffs against: the previous applied version's
-- inventory minus this one's is what it removes.
-- Expand-only: the column is nullable, a binary that predates it neither
-- reads nor writes it, and a receipt written without one prunes nothing.
ALTER TABLE "packages" ADD COLUMN IF NOT EXISTS "inventory_json" text NULL;
