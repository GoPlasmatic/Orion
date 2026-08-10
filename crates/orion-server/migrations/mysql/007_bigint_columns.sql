-- Widen integer columns to bigint, mirroring postgres/004 (proposal D11).
--
-- Every Rust model field for these columns is i64. Postgres was widened in
-- 004_bigint_columns.sql because sqlx-postgres refuses to read INT4 into i64;
-- MySQL was left behind because sqlx-mysql widens narrower integers on read,
-- so the divergence is silent rather than fatal.
--
-- Silent is the problem: `version` is a monotonic counter that would hit a
-- 2^31 ceiling on exactly one of the three backends, and the schema disagreed
-- with the model on that backend only. This is the same class of drift that
-- produced the Postgres INT4 incident.
--
-- MySQL has no view-dependency restriction on ALTER, so unlike postgres/004
-- this needs no DROP/CREATE VIEW dance and is a plain MODIFY.

ALTER TABLE `workflows`
    MODIFY `version` bigint NOT NULL,
    MODIFY `priority` bigint NOT NULL DEFAULT 0,
    MODIFY `rollout_percentage` bigint NOT NULL DEFAULT 100;

ALTER TABLE `channels`
    MODIFY `version` bigint NOT NULL,
    MODIFY `priority` bigint NOT NULL DEFAULT 0;

ALTER TABLE `trace_dlq`
    MODIFY `retry_count` bigint NOT NULL DEFAULT 0,
    MODIFY `max_retries` bigint NOT NULL DEFAULT 5;
