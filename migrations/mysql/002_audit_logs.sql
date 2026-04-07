-- Audit log table for persisting admin API mutation events.
CREATE TABLE IF NOT EXISTS `audit_logs` (
    `id` varchar(36) NOT NULL PRIMARY KEY,
    `principal` varchar(255) NOT NULL,
    `action` varchar(64) NOT NULL,
    `resource_type` varchar(64) NOT NULL,
    `resource_id` varchar(255) NOT NULL,
    `details` text,
    `created_at` timestamp NOT NULL DEFAULT CURRENT_TIMESTAMP
);

CREATE INDEX `idx_audit_logs_created_at` ON `audit_logs` (`created_at`);
CREATE INDEX `idx_audit_logs_resource` ON `audit_logs` (`resource_type`, `resource_id`);
