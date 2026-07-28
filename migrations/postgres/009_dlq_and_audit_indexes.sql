-- Indexes for the DLQ and audit-log list paths (proposals D20, D21).
--
-- trace_dlq: the operator list orders by created_at, which had no index, so it
-- full-scanned and filesorted the table holding the widest rows in the schema
-- (full request payloads). count() runs on every DLQ retry tick to refresh the
-- depth gauge, on the same unindexed predicate.
--
-- audit_logs: `principal` is the first query an auditor runs ("who did what")
-- and `action` the second; neither was indexed, against a table with 90-day
-- default retention. Both are composite with created_at so they also serve the
-- ORDER BY created_at DESC that every page applies.

CREATE INDEX "idx_trace_dlq_created_at" ON "trace_dlq" ("created_at");
CREATE INDEX "idx_trace_dlq_channel_created_at" ON "trace_dlq" ("channel", "created_at");
CREATE INDEX "idx_audit_logs_principal_created_at" ON "audit_logs" ("principal", "created_at");
CREATE INDEX "idx_audit_logs_action_created_at" ON "audit_logs" ("action", "created_at");
