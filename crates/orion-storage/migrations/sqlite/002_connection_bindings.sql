-- Connection bindings: which connection each dependency alias of a `workflow`
-- provider package calls, as canonical JSON (`{"alias": "connection_id"}`).
-- NULL for a connection with none, which is every connection over a built-in
-- driver. Expand-only: a binary that predates the column never writes it.
ALTER TABLE "connection_revisions" ADD COLUMN "bindings_json" text;
