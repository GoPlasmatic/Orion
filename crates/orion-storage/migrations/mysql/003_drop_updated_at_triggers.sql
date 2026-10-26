-- Drop the `updated_at` triggers on traces, trace_dlq and the cron tables.
--
-- Every UPDATE on these tables now sets `updated_at = CURRENT_TIMESTAMP`
-- itself (`storage::repositories::helpers::updated_now`), which is the value
-- the triggers wrote. The SQLite migration of the same name carries the notes.
DROP TRIGGER IF EXISTS trg_traces_updated_at;
DROP TRIGGER IF EXISTS trg_trace_dlq_updated_at;
DROP TRIGGER IF EXISTS trg_cron_schedule_state_updated_at;
DROP TRIGGER IF EXISTS trg_cron_occurrences_updated_at;
DROP TRIGGER IF EXISTS trg_cron_singletons_updated_at;
DROP TRIGGER IF EXISTS trg_cron_singleton_slots_updated_at;
