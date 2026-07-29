-- Suffix the last two JSON columns (proposal D26).
--
-- `workflows.tags` and `channels.methods` hold serialized JSON documents —
-- `["orders"]`, `["GET","POST"]` — exactly like `condition_json`,
-- `tasks_json`, `transport_config_json`, `config_json`, `input_json`,
-- `result_json`, `payload_json`, `metadata_json` and `task_trace_json`. They
-- were the only two without the suffix, so `_json` meant "decode me before
-- use" everywhere except two places, and a reader had no way to tell those
-- two apart from a plain scalar column without opening the row struct.
--
-- SQLite is the one backend where this is two lines. Since 3.25,
-- `ALTER TABLE ... RENAME COLUMN` rewrites the stored SQL of every trigger and
-- view that names the column, so `trg_workflows_active_immutable` (which
-- compares `OLD.tags`) and `trg_channels_active_immutable` (`OLD.methods`) are
-- edited in place and keep firing. `current_workflows` / `current_channels`
-- select `w.*` / `c.*` and resolve their column list at query time, so they
-- pick the new name up with no edit at all.
--
-- Postgres and MySQL store view target lists and trigger bodies as resolved
-- text and get none of that for free; see `postgres/013` and `mysql/011`.

ALTER TABLE "workflows" RENAME COLUMN "tags" TO "tags_json";
ALTER TABLE "channels" RENAME COLUMN "methods" TO "methods_json";
