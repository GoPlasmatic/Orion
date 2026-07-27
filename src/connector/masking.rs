//! Secret masking for connector configs returned by the admin API.
//!
//! Two independent rules, applied to the whole config tree rather than to a
//! fixed list of top-level keys:
//!
//!   1. **By key name** — a scalar under a secret-looking key is replaced
//!      wholesale. When the key itself names a credential bundle
//!      (`credentials`, `auth_token`, …) everything beneath it is masked too,
//!      whatever the child keys are called.
//!   2. **By value shape** — any string carrying URL userinfo has its password
//!      component replaced, at any depth. This is what keeps
//!      `redis://:PASSWORD@host` and `https://user:pass@es:9200` out of
//!      `GET /api/v1/admin/connectors` while leaving the endpoint itself
//!      readable, which the admin UI needs.

use serde_json::Value;

pub(crate) const MASK: &str = "******";

/// Substrings that make a key a secret wherever it appears in the tree.
/// Deliberately narrow: `key` alone is not here because it matches innocuous
/// names like `cache_key_fields` — it is handled as an exact match below.
const SECRET_KEY_SUBSTRINGS: &[&str] = &[
    "password",
    "passwd",
    "passphrase",
    "secret",
    "token",
    "credential",
    "api_key",
    "apikey",
    "private_key",
    "privatekey",
    "access_key",
    "accesskey",
    "signature",
];

/// Keys that are secrets only as an exact match.
const SECRET_KEY_EXACT: &[&str] = &[
    "key",
    "pwd",
    "cert",
    "certificate",
    "ssl_key",
    "sasl_key",
    "keyfile",
    "keytab",
    "authorization",
    "connection_string",
];

/// Whether a scalar stored under `key` should be replaced with [`MASK`].
fn is_secret_key(key: &str) -> bool {
    let lower = key.to_ascii_lowercase();
    SECRET_KEY_EXACT.contains(&lower.as_str())
        || SECRET_KEY_SUBSTRINGS.iter().any(|p| lower.contains(p))
}

/// Replace the password in a `scheme://user:password@host…` string, preserving
/// everything else byte-for-byte. Returns `None` when the string carries no
/// userinfo password, so credential-free URLs are left exactly as authored.
///
/// Hand-rolled rather than parsed with the `url` crate: schemes such as
/// `SASL_SSL://` are not valid URL schemes but do appear in Kafka broker
/// lists, and re-serialising a parsed URL rewrites strings that had nothing to
/// hide (`https://h` → `https://h/`).
fn redact_url_password(s: &str) -> Option<String> {
    let (scheme, rest) = s.split_once("://")?;
    if scheme.is_empty()
        || !scheme
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '-' | '.' | '_'))
    {
        return None;
    }
    // Userinfo lives before the first '/', '?' or '#' that ends the authority.
    let authority_end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
    let at = rest[..authority_end].rfind('@')?;
    let (user, _password) = rest[..at].split_once(':')?;
    Some(format!("{scheme}://{user}:{MASK}@{}", &rest[at + 1..]))
}

/// Walk the config tree masking secrets. `force` propagates down from a key
/// that names a credential bundle, so its children are masked regardless of
/// what they are called.
fn mask_in_place(value: &mut Value, key: Option<&str>, force: bool) {
    let secret = force || key.is_some_and(is_secret_key);
    match value {
        Value::Object(map) => {
            for (k, v) in map.iter_mut() {
                mask_in_place(v, Some(k.as_str()), secret);
            }
        }
        // Arrays inherit the parent key so `brokers: [...]` is still evaluated
        // against the name `brokers`.
        Value::Array(items) => {
            for item in items.iter_mut() {
                mask_in_place(item, key, secret);
            }
        }
        Value::Null => {}
        Value::String(s) => {
            if secret {
                *s = MASK.to_string();
            } else if let Some(redacted) = redact_url_password(s) {
                *s = redacted;
            }
        }
        // Numbers and booleans can still be secrets (a numeric PIN, say).
        other => {
            if secret {
                *other = Value::String(MASK.to_string());
            }
        }
    }
}

/// Mask sensitive fields in a connector's config_json for API responses.
pub fn mask_connector_secrets(config_json: &str) -> String {
    let Ok(mut val) = serde_json::from_str::<Value>(config_json) else {
        return config_json.to_string();
    };

    mask_in_place(&mut val, None, false);

    serde_json::to_string(&val).unwrap_or_else(|_| config_json.to_string())
}

/// Return a connector model with secrets masked.
pub fn mask_connector(
    connector: &crate::storage::models::Connector,
) -> crate::storage::models::Connector {
    let mut masked = connector.clone();
    masked.config_json = mask_connector_secrets(&masked.config_json);
    masked
}

/// Restore values that a client round-tripped through the masked read API
/// (F34).
///
/// A `GET` → edit-one-field → `PUT` cycle sends `"******"` back for every
/// secret the reader never saw, and a wholesale `config_json` write would
/// persist the mask *as* the credential. A field is therefore treated as
/// unchanged when the incoming value is exactly what a `GET` would have shown
/// for the stored value.
///
/// Defined by re-masking the stored config and comparing, rather than by
/// re-testing key names: that covers whole-value masks
/// (`"password": "******"`), the URL form (`"url": "redis://u:******@host"`),
/// and any masking rule added later, with no second copy of the logic to keep
/// in sync.
///
/// Anything still carrying a mask afterwards had no stored counterpart to
/// restore from — a genuinely new field, or a config whose shape changed — and
/// is left alone for [`find_masked_value`] to reject.
pub fn unmask_config(incoming: &mut Value, stored: &Value) {
    let mut masked_stored = stored.clone();
    mask_in_place(&mut masked_stored, None, false);
    restore_in_place(incoming, stored, &masked_stored);
}

fn restore_in_place(incoming: &mut Value, stored: &Value, masked: &Value) {
    match (incoming, stored, masked) {
        (Value::Object(inc), Value::Object(st), Value::Object(mk)) => {
            for (key, value) in inc.iter_mut() {
                if let (Some(stored_value), Some(masked_value)) = (st.get(key), mk.get(key)) {
                    restore_in_place(value, stored_value, masked_value);
                }
            }
        }
        // Masking is index-preserving, so positions line up. A reordered or
        // resized array simply stops matching and falls through to rejection.
        (Value::Array(inc), Value::Array(st), Value::Array(mk)) => {
            for ((value, stored_value), masked_value) in inc.iter_mut().zip(st).zip(mk) {
                restore_in_place(value, stored_value, masked_value);
            }
        }
        (inc, stored_value, masked_value) => {
            // `inc != stored_value` keeps this a no-op for unmasked fields,
            // and for the pathological case of a secret whose real value is
            // the mask string.
            if inc == masked_value && inc != stored_value {
                *inc = stored_value.clone();
            }
        }
    }
}

/// The dotted path of the first value that still carries the mask sentinel, or
/// `None` when the config is clean.
///
/// Used to reject writes that would persist `"******"` as a real credential —
/// on create, where there is nothing to restore from, and on update for
/// anything [`unmask_config`] could not match to a stored value.
pub fn find_masked_value(value: &Value) -> Option<String> {
    fn walk(value: &Value, path: &str, out: &mut Option<String>) {
        if out.is_some() {
            return;
        }
        match value {
            Value::Object(map) => {
                for (key, child) in map {
                    let child_path = if path.is_empty() {
                        key.clone()
                    } else {
                        format!("{path}.{key}")
                    };
                    walk(child, &child_path, out);
                }
            }
            Value::Array(items) => {
                for (i, item) in items.iter().enumerate() {
                    walk(item, &format!("{path}[{i}]"), out);
                }
            }
            // Either the whole value is the mask, or it is a URL whose
            // password component already reads as the mask — re-masking it
            // would be a no-op.
            Value::String(s)
                if s == MASK || redact_url_password(s).as_deref() == Some(s.as_str()) =>
            {
                *out = Some(path.to_string());
            }
            _ => {}
        }
    }

    let mut found = None;
    walk(value, "", &mut found);
    found
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_mask_connector_secrets_bearer_token() {
        let config = r#"{"type":"http","url":"https://api.example.com","auth":{"type":"bearer","token":"secret123"}}"#;
        let masked = mask_connector_secrets(config);
        let val: serde_json::Value = serde_json::from_str(&masked).expect("test");
        assert_eq!(val["auth"]["token"], "******");
    }

    #[test]
    fn test_mask_connector_secrets_basic_password() {
        let config = r#"{"type":"http","url":"https://api.example.com","auth":{"type":"basic","username":"user","password":"secret"}}"#;
        let masked = mask_connector_secrets(config);
        let val: serde_json::Value = serde_json::from_str(&masked).expect("test");
        assert_eq!(val["auth"]["password"], "******");
        // Username should NOT be masked
        assert_eq!(val["auth"]["username"], "user");
    }

    #[test]
    fn test_mask_connector_secrets_api_key() {
        let config = r#"{"type":"http","url":"https://api.example.com","auth":{"type":"apikey","key":"mysecretkey"}}"#;
        let masked = mask_connector_secrets(config);
        let val: serde_json::Value = serde_json::from_str(&masked).expect("test");
        assert_eq!(val["auth"]["key"], "******");
    }

    #[test]
    fn test_mask_connector_secrets_top_level_fields() {
        let config = r#"{"type":"http","url":"https://api.example.com","password":"top_secret","api_key":"ak123","token":"tk456","secret":"shhh"}"#;
        let masked = mask_connector_secrets(config);
        let val: serde_json::Value = serde_json::from_str(&masked).expect("test");
        assert_eq!(val["password"], "******");
        assert_eq!(val["api_key"], "******");
        assert_eq!(val["token"], "******");
        assert_eq!(val["secret"], "******");
        // A URL carrying no credentials stays readable — the admin UI needs to
        // show which endpoint a connector points at.
        assert_eq!(val["url"], "https://api.example.com");
    }

    #[test]
    fn url_userinfo_password_is_redacted() {
        let config = r#"{"type":"es","url":"https://elastic:s3cret@es.internal:9200"}"#;
        let masked = mask_connector_secrets(config);
        let val: serde_json::Value = serde_json::from_str(&masked).expect("test");
        assert_eq!(val["url"], "https://elastic:******@es.internal:9200");
    }

    #[test]
    fn url_userinfo_password_is_redacted_without_a_username() {
        let config =
            r#"{"type":"cache","backend":"redis","url":"redis://:hunter2@cache.internal:6379/0"}"#;
        let masked = mask_connector_secrets(config);
        let val: serde_json::Value = serde_json::from_str(&masked).expect("test");
        assert_eq!(val["url"], "redis://:******@cache.internal:6379/0");
        assert_eq!(val["backend"], "redis");
    }

    #[test]
    fn broker_list_entries_are_redacted() {
        let config = r#"{"type":"kafka","brokers":["sasl_ssl://svc:tok3n@b1.example:9093","b2.example:9092"],"topic":"orders"}"#;
        let masked = mask_connector_secrets(config);
        let val: serde_json::Value = serde_json::from_str(&masked).expect("test");
        assert_eq!(val["brokers"][0], "sasl_ssl://svc:******@b1.example:9093");
        // A plain host:port entry has nothing to hide.
        assert_eq!(val["brokers"][1], "b2.example:9092");
        assert_eq!(val["topic"], "orders");
    }

    #[test]
    fn nested_secrets_below_the_first_level_are_masked() {
        let config = r#"{"type":"http","url":"https://api.example.com","headers":{"authorization":"Bearer abc","x-tenant":"acme"},"extra":{"deep":{"client_secret":"cs123"}}}"#;
        let masked = mask_connector_secrets(config);
        let val: serde_json::Value = serde_json::from_str(&masked).expect("test");
        assert_eq!(val["headers"]["authorization"], "******");
        assert_eq!(val["headers"]["x-tenant"], "acme");
        assert_eq!(val["extra"]["deep"]["client_secret"], "******");
    }

    #[test]
    fn credential_bundles_mask_every_child() {
        // The bundle's children are not individually secret-looking, so only
        // the propagated flag saves them.
        let config = r#"{"type":"storage","credentials":{"id":"AKIAEXAMPLE","value":"wJalrXUtn"},"bucket":"assets"}"#;
        let masked = mask_connector_secrets(config);
        let val: serde_json::Value = serde_json::from_str(&masked).expect("test");
        assert_eq!(val["credentials"]["id"], "******");
        assert_eq!(val["credentials"]["value"], "******");
        assert_eq!(val["bucket"], "assets");
    }

    #[test]
    fn kafka_sasl_password_shape_is_masked() {
        // Mirrors the `[kafka.auth]` shape added in Phase 4.
        let config = r#"{"type":"kafka","brokers":["b:9092"],"topic":"t","auth":{"sasl_mechanism":"PLAIN","sasl_username":"svc","sasl_password":"p@ss"}}"#;
        let masked = mask_connector_secrets(config);
        let val: serde_json::Value = serde_json::from_str(&masked).expect("test");
        assert_eq!(val["auth"]["sasl_password"], "******");
        assert_eq!(val["auth"]["sasl_username"], "svc");
        assert_eq!(val["auth"]["sasl_mechanism"], "PLAIN");
    }

    #[test]
    fn innocuous_keys_are_left_alone() {
        let config = r#"{"type":"db","driver":"postgres","max_connections":10,"query_timeout_ms":5000,"cache_key_fields":["tenant"],"operations":{"read":true,"delete":false},"retry":{"max_retries":3}}"#;
        let masked = mask_connector_secrets(config);
        let val: serde_json::Value = serde_json::from_str(&masked).expect("test");
        assert_eq!(val["driver"], "postgres");
        assert_eq!(val["max_connections"], 10);
        assert_eq!(val["query_timeout_ms"], 5000);
        assert_eq!(val["cache_key_fields"][0], "tenant");
        assert_eq!(val["operations"]["read"], true);
        assert_eq!(val["operations"]["delete"], false);
        assert_eq!(val["retry"]["max_retries"], 3);
    }

    #[test]
    fn non_string_secret_values_are_masked() {
        let config = r#"{"type":"http","token":12345}"#;
        let masked = mask_connector_secrets(config);
        let val: serde_json::Value = serde_json::from_str(&masked).expect("test");
        assert_eq!(val["token"], "******");
    }

    #[test]
    fn redact_url_password_ignores_non_urls_and_credential_free_urls() {
        assert_eq!(redact_url_password("plain text"), None);
        assert_eq!(redact_url_password("https://example.com/a?b=c"), None);
        // A username with no password is an identity, not a secret.
        assert_eq!(redact_url_password("postgres://user@host/db"), None);
        // A '@' in the path must not be mistaken for userinfo.
        assert_eq!(redact_url_password("https://host/path@here"), None);
    }

    #[test]
    fn test_mask_connector_secrets_no_auth() {
        let config = r#"{"type":"http","url":"https://api.example.com"}"#;
        let masked = mask_connector_secrets(config);
        let val: serde_json::Value = serde_json::from_str(&masked).expect("test");
        assert_eq!(val["url"], "https://api.example.com");
    }

    #[test]
    fn test_mask_connector_secrets_invalid_json() {
        let config = "not valid json";
        let masked = mask_connector_secrets(config);
        assert_eq!(masked, config);
    }

    #[test]
    fn test_mask_connector_model() {
        use chrono::NaiveDate;
        let connector = crate::storage::models::Connector {
            id: "c1".to_string(),
            name: "test".to_string(),
            connector_type: "http".to_string(),
            config_json: r#"{"type":"http","url":"https://api.example.com","auth":{"type":"bearer","token":"secret"}}"#.to_string(),
            enabled: true,
            created_at: NaiveDate::from_ymd_opt(2025, 1, 1)
                .expect("test")
                .and_hms_opt(0, 0, 0)
                .expect("test"),
            updated_at: NaiveDate::from_ymd_opt(2025, 1, 1)
                .expect("test")
                .and_hms_opt(0, 0, 0)
                .expect("test"),
        };
        let masked = mask_connector(&connector);
        assert_eq!(masked.id, "c1");
        let val: serde_json::Value = serde_json::from_str(&masked.config_json).expect("test");
        assert_eq!(val["auth"]["token"], "******");
    }

    #[test]
    fn test_mask_connector_secrets_connection_string() {
        // Masked whole rather than userinfo-redacted: a DSN carries more than
        // the password (options, sslkey paths) and the UI never needed it.
        let config = r#"{"type":"db","connection_string":"postgres://user:pass@host/db","driver":"postgres"}"#;
        let masked = mask_connector_secrets(config);
        let val: serde_json::Value = serde_json::from_str(&masked).expect("test");
        assert_eq!(val["connection_string"], "******");
        assert_eq!(val["driver"], "postgres");
    }

    // ---------------------------------------------------------------
    // F34: mask round-trip on write
    // ---------------------------------------------------------------

    /// The exact GET → edit-one-field → PUT cycle an admin UI performs.
    #[test]
    fn test_unmask_restores_round_tripped_secret() {
        let stored: Value = serde_json::from_str(
            r#"{"type":"http","url":"https://api.example.com","timeout_secs":5,
                "auth":{"type":"bearer","token":"real-secret"}}"#,
        )
        .expect("test");
        // What GET returned, with one non-secret field edited.
        let mut incoming: Value =
            serde_json::from_str(&mask_connector_secrets(&stored.to_string())).expect("test");
        incoming["timeout_secs"] = serde_json::json!(30);

        unmask_config(&mut incoming, &stored);

        assert_eq!(incoming["auth"]["token"], "real-secret");
        assert_eq!(incoming["timeout_secs"], 30);
        assert_eq!(find_masked_value(&incoming), None);
    }

    /// F3 widened masking to `url`, so the userinfo form has to round-trip too.
    #[test]
    fn test_unmask_restores_url_password() {
        let stored: Value =
            serde_json::from_str(r#"{"type":"cache","url":"redis://admin:hunter2@redis:6379"}"#)
                .expect("test");
        let mut incoming: Value =
            serde_json::from_str(&mask_connector_secrets(&stored.to_string())).expect("test");
        assert_eq!(incoming["url"], "redis://admin:******@redis:6379");

        unmask_config(&mut incoming, &stored);

        assert_eq!(incoming["url"], "redis://admin:hunter2@redis:6379");
    }

    /// Masking is index-preserving, so a broker list round-trips per element.
    #[test]
    fn test_unmask_restores_array_elements() {
        let stored: Value = serde_json::from_str(
            r#"{"type":"kafka","brokers":["SASL_SSL://u:p1@b1:9093","plaintext://b2:9092"]}"#,
        )
        .expect("test");
        let mut incoming: Value =
            serde_json::from_str(&mask_connector_secrets(&stored.to_string())).expect("test");

        unmask_config(&mut incoming, &stored);

        assert_eq!(incoming["brokers"][0], "SASL_SSL://u:p1@b1:9093");
        assert_eq!(incoming["brokers"][1], "plaintext://b2:9092");
    }

    /// A real edit must survive: only values that still equal the mask are
    /// restored, never a value the caller actually changed.
    #[test]
    fn test_unmask_leaves_a_genuine_new_secret_alone() {
        let stored: Value =
            serde_json::from_str(r#"{"auth":{"token":"old-secret"}}"#).expect("test");
        let mut incoming: Value =
            serde_json::from_str(r#"{"auth":{"token":"new-secret"}}"#).expect("test");

        unmask_config(&mut incoming, &stored);

        assert_eq!(incoming["auth"]["token"], "new-secret");
    }

    /// A mask with no stored counterpart cannot be restored, so it must remain
    /// findable — the handler turns that into a 400 rather than persisting it.
    #[test]
    fn test_unmask_leaves_unmatched_mask_for_rejection() {
        let stored: Value = serde_json::from_str(r#"{"auth":{"token":"old"}}"#).expect("test");
        let mut incoming: Value =
            serde_json::from_str(r#"{"auth":{"token":"old"},"password":"******"}"#).expect("test");

        unmask_config(&mut incoming, &stored);

        assert_eq!(find_masked_value(&incoming).as_deref(), Some("password"));
    }

    #[test]
    fn test_find_masked_value_reports_a_dotted_path() {
        let config: Value = serde_json::from_str(r#"{"a":{"b":[{"c":"******"}]}}"#).expect("test");
        assert_eq!(find_masked_value(&config).as_deref(), Some("a.b[0].c"));
    }

    #[test]
    fn test_find_masked_value_detects_the_url_form() {
        let config: Value =
            serde_json::from_str(r#"{"url":"redis://admin:******@redis:6379"}"#).expect("test");
        assert_eq!(find_masked_value(&config).as_deref(), Some("url"));
    }

    #[test]
    fn test_find_masked_value_ignores_a_clean_config() {
        let config: Value = serde_json::from_str(
            r#"{"url":"https://api.example.com","auth":{"token":"real"},"n":6}"#,
        )
        .expect("test");
        assert_eq!(find_masked_value(&config), None);
    }
}
