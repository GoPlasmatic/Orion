-- Drop the `updated_at` triggers on traces, trace_dlq and the cron tables,
-- and the function they shared.
--
-- Every UPDATE on these tables now sets `updated_at = CURRENT_TIMESTAMP`
-- itself (`storage::repositories::helpers::updated_now`), which is the value
-- `update_updated_at_column()` wrote. The SQLite migration of the same name
-- carries the notes; nothing else uses the function.
DROP TRIGGER IF EXISTS trg_traces_updated_at ON traces;
DROP TRIGGER IF EXISTS trg_trace_dlq_updated_at ON trace_dlq;
DROP TRIGGER IF EXISTS trg_cron_schedule_state_updated_at ON cron_schedule_state;
DROP TRIGGER IF EXISTS trg_cron_occurrences_updated_at ON cron_occurrences;
DROP TRIGGER IF EXISTS trg_cron_singletons_updated_at ON cron_singletons;
DROP TRIGGER IF EXISTS trg_cron_singleton_slots_updated_at ON cron_singleton_slots;
DROP FUNCTION IF EXISTS update_updated_at_column();
