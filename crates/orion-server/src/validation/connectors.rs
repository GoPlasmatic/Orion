use crate::connector::{ConnectorConfig, ConnectorType};
use crate::errors::OrionError;
use crate::storage::repositories::connectors::{CreateConnectorRequest, UpdateConnectorRequest};

use super::common::{validate_id, validate_name};

/// Validate a connector `config` blob against an explicit type. Public so the
/// update handler can validate a config-only update against the stored
/// connector's type (R4).
pub fn validate_connector_config(
    connector_type: ConnectorType,
    config: &serde_json::Value,
) -> Result<(), OrionError> {
    let type_str = connector_type.as_str();
    // Inject the type field so we can deserialize as the tagged enum
    let mut config_with_type = config.clone();
    if let Some(obj) = config_with_type.as_object_mut() {
        obj.insert(
            "type".to_string(),
            serde_json::Value::String(type_str.to_string()),
        );
    } else {
        return Err(OrionError::validation(
            "Connector config must be a JSON object".to_string(),
        ));
    }

    let parsed: ConnectorConfig = serde_json::from_value(config_with_type).map_err(|e| {
        OrionError::validation(format!(
            "Invalid connector config for type '{type_str}': {e}"
        ))
    })?;

    validate_operation_gate_keys(connector_type, config)?;
    validate_retry_keys(connector_type, config)?;

    // S6: every variant's endpoint gets a scheme allow-list, not just HTTP's.
    // Schemes only — the private-address check runs on the pool-open paths,
    // because storing a connector must not depend on DNS.
    super::endpoints::validate_endpoint_schemes(&parsed)?;

    // For HTTP connectors, validate the URL scheme
    if let ConnectorConfig::Http(http_config) = &parsed
        && !http_config.url.is_empty()
    {
        let parsed_url = url::Url::parse(&http_config.url).map_err(|e| {
            OrionError::validation(format!("Invalid connector URL '{}': {e}", http_config.url))
        })?;
        let scheme = parsed_url.scheme();
        if scheme != "http" && scheme != "https" {
            return Err(OrionError::validation(format!(
                "Connector URL must use http or https scheme, got '{scheme}'"
            )));
        }
    }

    // Retry counts are exponents in the backoff schedule (2^attempt), so an
    // unbounded value is a config-reachable multi-hour stall, and arithmetic
    // on it has to stay overflow-safe. Same bound Q4 put on dlq_max_retries.
    // Only the HTTP connector carries a retry policy — F7 removed the inert
    // copies on db and es, so validating them here would have been validating
    // a field with no consumer.
    if let ConnectorConfig::Http(c) = &parsed
        && c.retry.max_retries > 16
    {
        return Err(OrionError::validation(format!(
            "retry.max_retries must be <= 16 (backoff doubles per attempt), got {}",
            c.retry.max_retries
        )));
    }

    // SMTP: the fields a send cannot proceed without are checked at the door.
    // `from` may be a secret-style reference only in theory — sender addresses
    // are not secrets — so a literal parse failure is a hard error here rather
    // than a first-send surprise.
    if let ConnectorConfig::Smtp(smtp) = &parsed {
        if smtp.port == 0 {
            return Err(OrionError::validation(
                "SMTP 'port' must be nonzero".to_string(),
            ));
        }
        if smtp.from.trim().is_empty() {
            return Err(OrionError::validation(
                "SMTP connector requires 'from' (the default sender address)".to_string(),
            ));
        }
        if let Err(e) = crate::engine::functions::send_email::parse_mailbox("from", &smtp.from) {
            return Err(OrionError::validation(e.to_string()));
        }
        if let crate::connector::SmtpAuth::Basic { username, .. } = &smtp.auth
            && username.trim().is_empty()
        {
            return Err(OrionError::validation(
                "SMTP auth type 'basic' requires a non-empty 'username'".to_string(),
            ));
        }
    }

    // F22e: an HTTP connector's operation gate is a method allow-list, so a
    // typo in it would not fail closed loudly — it would refuse every call at
    // request time, one workflow at a time. Check it at the door instead,
    // against the methods `http_call` can actually issue.
    if let ConnectorConfig::Http(c) = &parsed {
        for method in &c.operations.methods {
            if !crate::connector::VALID_HTTP_METHODS
                .iter()
                .any(|valid| valid.eq_ignore_ascii_case(method))
            {
                return Err(OrionError::validation(format!(
                    "Invalid HTTP method '{method}' in operations.methods. Must be \
                     one of: {}",
                    crate::connector::VALID_HTTP_METHODS.join(", ")
                )));
            }
        }
    }

    // For Cache connectors, validate backend and url requirement
    if let ConnectorConfig::Cache(cache_config) = &parsed {
        if !crate::connector::VALID_CACHE_BACKENDS.contains(&cache_config.backend.as_str()) {
            return Err(OrionError::validation(format!(
                "Invalid cache backend '{}'. Must be one of: {}",
                cache_config.backend,
                crate::connector::VALID_CACHE_BACKENDS.join(", ")
            )));
        }
        if cache_config.backend == "redis"
            && cache_config
                .url
                .as_ref()
                .is_none_or(|u| u.trim().is_empty())
        {
            return Err(OrionError::validation(
                "Cache connector with backend='redis' requires a non-empty 'url'".to_string(),
            ));
        }
    }

    Ok(())
}

/// F22e: refuse an `operations` object carrying a key this connector type does
/// not read.
///
/// The gate structs are `#[serde(default)]` and connector configs do not use
/// `deny_unknown_fields` (legacy rows must keep loading), so a misspelled gate
/// key deserializes into a fully open gate — `{"operations": {"writes": false}}`
/// on a cache would answer 201 and gate nothing. The values are already
/// checked here for the HTTP method allow-list; this is the same door for the
/// keys, for every type.
/// The keys `RetryConfig` reads. Mirrors the struct the way the gate lists
/// mirror theirs; `retry_allowed_keys_match_retry_config` below pins the two
/// together, so adding a `RetryConfig` field without updating this list is a
/// test failure rather than a valid config the boundary rejects.
const ALLOWED_RETRY_KEYS: &[&str] = &["max_retries", "retry_delay_ms"];

/// F60: the same door as [`validate_operation_gate_keys`], for `retry`.
/// `RetryConfig` is all-`serde(default)` with no `deny_unknown_fields`
/// (legacy rows must keep loading), so `{"retry": {"max_attempts": 5}}`
/// deserialized into the default policy and changed nothing — the operator
/// believed retries were configured. And only the HTTP connector *reads*
/// `retry` at all (F7 removed the inert copies on db and es), so a
/// correctly-spelled block on any other type is the same
/// believed-configured-but-inert mistake and is refused whole. Refused at
/// the CRUD boundary only; stored rows keep loading, exactly like the gate
/// check above.
fn validate_retry_keys(
    connector_type: ConnectorType,
    config: &serde_json::Value,
) -> Result<(), OrionError> {
    let Some(retry) = config.get("retry") else {
        return Ok(());
    };
    if !matches!(connector_type, ConnectorType::Http) {
        return Err(OrionError::validation(format!(
            "Connector type '{connector_type}' has no retry policy — only `http` \
             connectors read 'retry', so this block would be silently ignored. \
             Remove it, or configure retries on the calling workflow instead."
        )));
    }
    let Some(object) = retry.as_object() else {
        return Err(OrionError::validation(format!(
            "Connector 'retry' must be a JSON object with keys: {}",
            ALLOWED_RETRY_KEYS.join(", ")
        )));
    };
    let unknown: Vec<&str> = object
        .keys()
        .map(String::as_str)
        .filter(|key| !ALLOWED_RETRY_KEYS.contains(key))
        .collect();
    if unknown.is_empty() {
        return Ok(());
    }
    Err(OrionError::validation(format!(
        "Connector 'retry' has unrecognised key(s) {unknown:?} — a key the retry \
         policy does not read silently leaves the default policy in place. \
         Valid keys: {}",
        ALLOWED_RETRY_KEYS.join(", ")
    )))
}

fn validate_operation_gate_keys(
    connector_type: ConnectorType,
    config: &serde_json::Value,
) -> Result<(), OrionError> {
    let Some(operations) = config.get("operations") else {
        return Ok(());
    };
    let Some(object) = operations.as_object() else {
        return Err(OrionError::validation(format!(
            "Connector 'operations' must be a JSON object of operation gates, one of: {}",
            connector_type.operation_gate_keys().join(", ")
        )));
    };
    let allowed = connector_type.operation_gate_keys();
    let unknown: Vec<&str> = object
        .keys()
        .map(String::as_str)
        .filter(|key| !allowed.contains(key))
        .collect();
    if unknown.is_empty() {
        return Ok(());
    }
    Err(OrionError::validation(format!(
        "Unknown operation gate(s) {:?} for connector type '{connector_type}'. A gate \
         that is not read leaves the operation allowed, so this would be a silent \
         no-op — must be one of: {}",
        unknown,
        allowed.join(", ")
    )))
}

pub fn validate_create_connector(req: &CreateConnectorRequest) -> Result<(), OrionError> {
    if let Some(ref id) = req.id {
        validate_id(id, "connector.id")?;
    }
    validate_name(&req.name, "connector.name")?;
    // connector_type itself is now validated by serde at deserialization —
    // unknown values like "grpc" produce a 400 before this function runs.
    validate_connector_config(req.connector_type, &req.config)?;
    // F34: on create there is no stored value to restore a mask from, so a
    // mask here is always a copied-from-a-GET mistake.
    reject_masked_values(&req.config)?;
    Ok(())
}

/// Refuse to persist the read API's mask sentinel as a real credential (F34).
pub fn reject_masked_values(config: &serde_json::Value) -> Result<(), OrionError> {
    if let Some(path) = crate::connector::find_masked_value(config) {
        return Err(OrionError::validation(format!(
            "Connector config field '{path}' is the masked placeholder that \
             GET /api/v1/admin/connectors returns, not a real value. Send the \
             actual secret, or omit the field to keep the stored one."
        )));
    }
    Ok(())
}

/// Validate the parts of an update that need no database read.
///
/// The config is **not** validated here. It has to be checked after the
/// handler has restored masked fields from the stored row (F34) and resolved
/// the effective type — which for a config-only update is the stored one (R4).
/// Validating a config that still reads `"url": "******"` would reject a legal
/// edit, so the handler owns that step.
pub fn validate_update_connector(req: &UpdateConnectorRequest) -> Result<(), OrionError> {
    if let Some(ref name) = req.name {
        validate_name(name, "connector.name")?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    // Note: connector_type string validation (rejecting "grpc", "" etc.) is
    // now enforced at serde deserialization time on the DTO (A4). The
    // previous `validate_connector_type` helper and its tests have been
    // removed because constructing the typed `ConnectorType` enum directly
    // bypasses the wire-format check that the test was exercising. The
    // integration test `invalid_connector_type_emits_enum_mismatch_with_expected_got`
    // in tests/error_envelope_test.rs covers the end-to-end behaviour.

    #[test]
    fn test_connector_config_http_valid() {
        let config = json!({
            "url": "https://example.com/api",
            "method": "POST"
        });
        assert!(validate_connector_config(ConnectorType::Http, &config).is_ok());
    }

    /// F60: a misspelled retry key deserialized into the default policy and
    /// changed nothing — refused at the boundary, like the operation gates.
    #[test]
    fn a_misspelled_retry_key_is_refused_and_named() {
        let config = json!({
            "url": "https://example.com/api",
            "retry": {"max_attempts": 5}
        });
        let err = validate_connector_config(ConnectorType::Http, &config)
            .expect_err("unknown retry key must not be silently ignored");
        let message = err.client_message();
        assert!(message.contains("max_attempts"), "{message}");
        assert!(message.contains("max_retries"), "{message}");

        let config = json!({
            "url": "https://example.com/api",
            "retry": {"max_retries": 5, "retry_delay_ms": 100}
        });
        assert!(validate_connector_config(ConnectorType::Http, &config).is_ok());
    }

    #[test]
    fn test_connector_config_http_invalid_scheme() {
        let config = json!({
            "url": "ftp://example.com/api",
            "method": "POST"
        });
        assert!(validate_connector_config(ConnectorType::Http, &config).is_err());
    }

    #[test]
    fn test_connector_config_invalid_structure() {
        let config = json!("not an object");
        assert!(validate_connector_config(ConnectorType::Http, &config).is_err());
    }

    #[test]
    fn test_connector_config_http_empty_url() {
        let config = json!({"url": ""});
        // Empty URL should be fine (passes URL validation skip)
        assert!(validate_connector_config(ConnectorType::Http, &config).is_ok());
    }

    #[test]
    fn test_connector_config_http_invalid_url() {
        let config = json!({"url": "not a valid url"});
        assert!(validate_connector_config(ConnectorType::Http, &config).is_err());
    }

    #[test]
    fn test_validate_create_connector_with_id() {
        let req = CreateConnectorRequest {
            tags: vec![],
            id: Some("my-conn-1".to_string()),
            name: "My Connector".to_string(),
            connector_type: ConnectorType::Http,
            config: json!({"url": "https://example.com"}),
            enabled: None,
        };
        assert!(validate_create_connector(&req).is_ok());
    }

    #[test]
    fn test_validate_create_connector_invalid_id() {
        let req = CreateConnectorRequest {
            tags: vec![],
            id: Some("bad id!".to_string()),
            name: "My Connector".to_string(),
            connector_type: ConnectorType::Http,
            config: json!({"url": "https://example.com"}),
            enabled: None,
        };
        assert!(validate_create_connector(&req).is_err());
    }

    #[test]
    fn test_validate_create_connector_empty_name() {
        let req = CreateConnectorRequest {
            tags: vec![],
            id: None,
            name: "".to_string(),
            connector_type: ConnectorType::Http,
            config: json!({"url": "https://example.com"}),
            enabled: None,
        };
        assert!(validate_create_connector(&req).is_err());
    }

    #[test]
    fn test_validate_update_connector_with_name() {
        let req = UpdateConnectorRequest {
            tags: None,
            name: Some("Updated Name".to_string()),
            connector_type: None,
            config: None,
            enabled: None,
        };
        assert!(validate_update_connector(&req).is_ok());
    }

    #[test]
    fn test_validate_update_connector_invalid_name() {
        let req = UpdateConnectorRequest {
            tags: None,
            name: Some("   ".to_string()),
            connector_type: None,
            config: None,
            enabled: None,
        };
        assert!(validate_update_connector(&req).is_err());
    }

    #[test]
    fn test_validate_update_connector_type_only() {
        let req = UpdateConnectorRequest {
            tags: None,
            name: None,
            connector_type: Some(ConnectorType::Http),
            config: None,
            enabled: None,
        };
        assert!(validate_update_connector(&req).is_ok());
    }

    #[test]
    fn test_validate_update_connector_type_and_config() {
        let req = UpdateConnectorRequest {
            tags: None,
            name: None,
            connector_type: Some(ConnectorType::Http),
            config: Some(json!({"url": "https://example.com"})),
            enabled: None,
        };
        assert!(validate_update_connector(&req).is_ok());
    }

    /// The config is no longer checked here: the handler validates it after
    /// restoring masked fields from the stored row (F34), because a config
    /// that still reads `"url": "******"` is not the config being persisted.
    /// The rejection itself is covered end-to-end by
    /// `admin_connectors_test::test_update_connector_type_and_invalid_config_rejected`.
    #[test]
    fn test_validate_update_connector_defers_config_to_the_handler() {
        let req = UpdateConnectorRequest {
            tags: None,
            name: None,
            connector_type: Some(ConnectorType::Http),
            config: Some(json!("not an object")),
            enabled: None,
        };
        assert!(validate_update_connector(&req).is_ok());
        // …and the check the handler runs is the one that rejects it.
        assert!(
            validate_connector_config(ConnectorType::Http, &json!("not an object")).is_err(),
            "the handler's post-unmask validation must still reject this"
        );
    }

    #[test]
    fn test_validate_update_connector_no_fields() {
        let req = UpdateConnectorRequest {
            tags: None,
            name: None,
            connector_type: None,
            config: None,
            enabled: None,
        };
        assert!(validate_update_connector(&req).is_ok());
    }

    // R4: config-without-type passes `validate_update_connector` (the stored
    // type is not available here) — the update handler validates it against
    // the stored connector's type via `validate_connector_config`.

    #[test]
    fn test_connector_config_db_missing_connection_string() {
        assert!(validate_connector_config(ConnectorType::Db, &json!({})).is_err());
    }

    #[test]
    fn test_connector_config_db_valid() {
        let config = json!({"connection_string": "sqlite::memory:"});
        assert!(validate_connector_config(ConnectorType::Db, &config).is_ok());
    }

    /// F22e: a method allow-list is only useful if a typo in it is caught
    /// here. `"GTE"` would otherwise persist happily and refuse every call
    /// through the connector at request time.
    #[test]
    fn http_method_allow_list_rejects_a_method_http_call_cannot_issue() {
        let config = json!({
            "url": "https://example.com",
            "operations": { "methods": ["GET", "GTE"] }
        });
        let err = validate_connector_config(ConnectorType::Http, &config)
            .expect_err("a misspelled method must not be persisted");
        assert!(err.to_string().contains("GTE"), "{err}");
    }

    #[test]
    fn http_method_allow_list_accepts_the_supported_methods_in_any_case() {
        let config = json!({
            "url": "https://example.com",
            "operations": { "methods": ["get", "POST", "Put", "PATCH", "delete"] }
        });
        assert!(validate_connector_config(ConnectorType::Http, &config).is_ok());
    }

    #[test]
    fn test_connector_config_cache_invalid_backend() {
        let config = json!({"backend": "memcached"});
        assert!(validate_connector_config(ConnectorType::Cache, &config).is_err());
    }

    #[test]
    fn test_connector_config_cache_redis_requires_url() {
        let config = json!({"backend": "redis"});
        assert!(validate_connector_config(ConnectorType::Cache, &config).is_err());
        let config = json!({"backend": "redis", "url": "redis://localhost:6379"});
        assert!(validate_connector_config(ConnectorType::Cache, &config).is_ok());
    }

    /// `ALLOWED_RETRY_KEYS` mirrors `RetryConfig` by hand; this pins the two
    /// together the way `operation_gate_keys_match_the_gate_structs` pins the
    /// gate lists. Without it, a new `RetryConfig` field makes the F60
    /// boundary reject a perfectly valid config until someone remembers the
    /// list — a hard 400, not just a stale message.
    #[test]
    fn retry_allowed_keys_match_retry_config() {
        let value =
            serde_json::to_value(crate::connector::RetryConfig::default()).expect("serialize");
        let mut from_struct: Vec<&str> = value
            .as_object()
            .expect("RetryConfig serializes as an object")
            .keys()
            .map(String::as_str)
            .collect();
        from_struct.sort_unstable();
        let mut allowed = ALLOWED_RETRY_KEYS.to_vec();
        allowed.sort_unstable();
        assert_eq!(
            allowed, from_struct,
            "ALLOWED_RETRY_KEYS has drifted from RetryConfig's fields"
        );
    }
}
