-- See sqlite/003_task_trace.sql for rationale.
ALTER TABLE traces ADD COLUMN task_trace_json TEXT;
