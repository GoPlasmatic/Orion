-- Cluster state the nodes of a cluster share through the database: the
-- default deduplication store, the default response cache and namespace
-- counters (`cluster_kv`), and fleet-wide rate-limit windows
-- (`cluster_rate_windows`). Keys are SHA-256 hex of the store's own key, so
-- an arbitrarily long cache key fits a fixed-width primary key. Expiry is
-- judged on the database's clock, the one clock every node shares.
CREATE TABLE IF NOT EXISTS "cluster_kv" (
    "key_hash" text NOT NULL PRIMARY KEY,
    "value" text NOT NULL,
    "expires_at" timestamp
);
CREATE INDEX IF NOT EXISTS "idx_cluster_kv_expires_at" ON "cluster_kv" ("expires_at");

CREATE TABLE IF NOT EXISTS "cluster_rate_windows" (
    "limiter_key" text NOT NULL,
    "window_start" bigint NOT NULL,
    "hits" bigint NOT NULL,
    PRIMARY KEY ("limiter_key", "window_start")
);
