-- Drop the `updated_at` triggers on traces, trace_dlq and the cron tables.
--
-- Every UPDATE on these tables now sets `updated_at = CURRENT_TIMESTAMP`
-- itself (`storage::repositories::helpers::updated_now`), which is the value
-- the triggers wrote. Here each trigger was an AFTER UPDATE running a second
-- UPDATE of the same row, so a trace's life cost twice the statements it
-- needed, and an UPDATE … RETURNING returned the old `updated_at`.
--
-- Contract step: during a rolling deploy a binary older than this migration
-- leaves `updated_at` unchanged on its UPDATEs. Nothing reads the column for
-- anything but display and the `sort_by=updated_at` listing order.
DROP TRIGGER IF EXISTS trg_traces_updated_at;
DROP TRIGGER IF EXISTS trg_trace_dlq_updated_at;
DROP TRIGGER IF EXISTS trg_cron_schedule_state_updated_at;
DROP TRIGGER IF EXISTS trg_cron_occurrences_updated_at;
DROP TRIGGER IF EXISTS trg_cron_singletons_updated_at;
DROP TRIGGER IF EXISTS trg_cron_singleton_slots_updated_at;
