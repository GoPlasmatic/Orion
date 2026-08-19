-- connector_oauth_state (#268): runtime token state for managed OAuth2
-- connector auth — the rotated refresh token plus the current access token
-- and its expiry, so cluster nodes adopt a fresh token instead of racing the
-- rotation. Keyed by connector name; `fingerprint` hashes the connector's
-- oauth2 auth block, so editing the connector invalidates stale state (that
-- is also the burned-token recovery story: re-seed the config). `state_json`
-- is encrypted with storage.connector_encryption_key when set, exactly like
-- connectors.config_json. Deliberately NOT a column on `connectors`: that
-- table is declarative, user-authored config — export/import, masking and
-- content hashing all treat it that way — and runtime state mutating it
-- would race its owner.

CREATE TABLE IF NOT EXISTS "connector_oauth_state" (
    "connector_name" text NOT NULL PRIMARY KEY,
    "fingerprint" text NOT NULL,
    "state_json" text NOT NULL,
    "updated_at" timestamp NOT NULL DEFAULT CURRENT_TIMESTAMP
);
