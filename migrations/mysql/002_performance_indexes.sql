-- Composite index for trace pagination queries that filter by status + channel + mode
CREATE INDEX `idx_traces_status_channel_mode` ON `traces` (`status`, `channel`, `mode`, `created_at`);

-- Composite index for mode + created_at sorting
CREATE INDEX `idx_traces_mode_created_at` ON `traces` (`mode`, `created_at`);
