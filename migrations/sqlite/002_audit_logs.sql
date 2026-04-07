-- Audit log table for persisting admin API mutation events.
CREATE TABLE IF NOT EXISTS "audit_logs" (
    "id" text NOT NULL PRIMARY KEY,
    "principal" text NOT NULL,
    "action" text NOT NULL,
    "resource_type" text NOT NULL,
    "resource_id" text NOT NULL,
    "details" text,
    "created_at" timestamp_text NOT NULL DEFAULT CURRENT_TIMESTAMP
);

CREATE INDEX "idx_audit_logs_created_at" ON "audit_logs" ("created_at");
CREATE INDEX "idx_audit_logs_resource" ON "audit_logs" ("resource_type", "resource_id");
