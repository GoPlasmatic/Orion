-- Package receipts (K14): the one piece of server-side package state.
-- One row per (package name, package version) ever applied to this
-- deployment. The PUT endpoint enforces applied-version immutability against
-- these rows: a version in state 'applied' never changes content again, so a
-- changed artifact reusing an applied version is rejected with a 409. History
-- is kept; "current" is derived (newest updated_at in state 'applied'), never
-- stored, so there is no pointer to race on.
-- updated_at is stamped by the repository (sql_now), not a trigger: every
-- write path goes through the one conditional-update repository method.
-- Key columns are varchar to fit MySQL's index length limits, sized to the
-- route-layer caps (name/version <= 128/64 chars, validated before storage).

CREATE TABLE IF NOT EXISTS `packages` (
    `name` varchar(128) NOT NULL,
    `version` varchar(64) NOT NULL,
    `content_hash` varchar(128) NOT NULL,
    `state` varchar(16) NOT NULL DEFAULT 'staged',
    `principal` varchar(128) NOT NULL DEFAULT '',
    `created_at` datetime NOT NULL DEFAULT CURRENT_TIMESTAMP,
    `updated_at` datetime NOT NULL DEFAULT CURRENT_TIMESTAMP,
    PRIMARY KEY (`name`, `version`)
);
