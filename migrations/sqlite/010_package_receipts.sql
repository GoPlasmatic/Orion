-- Package receipts (K14): the one piece of server-side package state.
-- One row per (package name, package version) ever applied to this
-- deployment. The PUT endpoint enforces applied-version immutability against
-- these rows: a version in state 'applied' never changes content again, so a
-- changed artifact reusing an applied version is rejected with a 409. History
-- is kept; "current" is derived (newest updated_at in state 'applied'), never
-- stored, so there is no pointer to race on.
-- updated_at is stamped by the repository (sql_now), not a trigger: every
-- write path goes through the one conditional-update repository method.

CREATE TABLE IF NOT EXISTS "packages" (
    "name" text NOT NULL,
    "version" text NOT NULL,
    "content_hash" text NOT NULL,
    "state" text NOT NULL DEFAULT 'staged',
    "principal" text NOT NULL DEFAULT '',
    "created_at" timestamp NOT NULL DEFAULT CURRENT_TIMESTAMP,
    "updated_at" timestamp NOT NULL DEFAULT CURRENT_TIMESTAMP,
    PRIMARY KEY ("name", "version")
);
