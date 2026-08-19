-- connector_oauth_state (#268): runtime token state for managed OAuth2
-- connector auth — see the sqlite migration of the same name for the full
-- rationale (rotation persistence, fingerprint invalidation, cluster
-- adoption; encrypted like connectors.config_json when the key is set).

CREATE TABLE IF NOT EXISTS "connector_oauth_state" (
    "connector_name" text NOT NULL PRIMARY KEY,
    "fingerprint" text NOT NULL,
    "state_json" text NOT NULL,
    "updated_at" timestamp NOT NULL DEFAULT CURRENT_TIMESTAMP
);
