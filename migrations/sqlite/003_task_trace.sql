-- Optional per-task trace JSON. Populated by the engine when a channel
-- has `config.tracing.task_details = true` so workflow authors can see
-- intermediate inputs/outputs for each task. NULL on production rows
-- where the channel didn't opt in — keeps the row size at zero overhead.
ALTER TABLE traces ADD COLUMN task_trace_json TEXT;
