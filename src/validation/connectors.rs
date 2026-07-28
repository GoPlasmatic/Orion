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
        return Err(OrionError::BadRequest(
            "Connector config must be a JSON object".to_string(),
        ));
    }

    let parsed: ConnectorConfig = serde_json::from_value(config_with_type).map_err(|e| {
        OrionError::BadRequest(format!(
            "Invalid connector config for type '{type_str}': {e}"
        ))
    })?;

    // For HTTP connectors, validate the URL scheme
    if let ConnectorConfig::Http(http_config) = &parsed
        && !http_config.url.is_empty()
    {
        let parsed_url = url::Url::parse(&http_config.url).map_err(|e| {
            OrionError::BadRequest(format!("Invalid connector URL '{}': {e}", http_config.url))
        })?;
        let scheme = parsed_url.scheme();
        if scheme != "http" && scheme != "https" {
            return Err(OrionError::BadRequest(format!(
                "Connector URL must use http or https scheme, got '{scheme}'"
            )));
        }
    }

    // Retry counts are exponents in the backoff schedule (2^attempt), so an
    // unbounded value is a config-reachable multi-hour stall, and arithmetic
    // on it has to stay overflow-safe. Same bound Q4 put on dlq_max_retries.
    if let Some(retry) = match &parsed {
        ConnectorConfig::Http(c) => Some(&c.retry),
        ConnectorConfig::Db(c) => Some(&c.retry),
        ConnectorConfig::Es(c) => Some(&c.retry),
        _ => None,
    } && retry.max_retries > 16
    {
        return Err(OrionError::BadRequest(format!(
            "retry.max_retries must be <= 16 (backoff doubles per attempt), got {}",
            retry.max_retries
        )));
    }

    // For Cache connectors, validate backend and url requirement
    if let ConnectorConfig::Cache(cache_config) = &parsed {
        if !crate::connector::VALID_CACHE_BACKENDS.contains(&cache_config.backend.as_str()) {
            return Err(OrionError::BadRequest(format!(
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
            return Err(OrionError::BadRequest(
                "Cache connector with backend='redis' requires a non-empty 'url'".to_string(),
            ));
        }
    }

    Ok(())
}

pub fn validate_create_connector(req: &CreateConnectorRequest) -> Result<(), OrionError> {
    if let Some(ref id) = req.id {
        validate_id(id)?;
    }
    validate_name(&req.name, "Name")?;
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
        return Err(OrionError::BadRequest(format!(
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
        validate_name(name, "Name")?;
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
    // in tests/error_envelope_test.rs covers the end-to-end behavior.

    #[test]
    fn test_connector_config_http_valid() {
        let config = json!({
            "url": "https://example.com/api",
            "method": "POST"
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
            id: Some("my-conn-1".to_string()),
            name: "My Connector".to_string(),
            connector_type: ConnectorType::Http,
            config: json!({"url": "https://example.com"}),
        };
        assert!(validate_create_connector(&req).is_ok());
    }

    #[test]
    fn test_validate_create_connector_invalid_id() {
        let req = CreateConnectorRequest {
            id: Some("bad id!".to_string()),
            name: "My Connector".to_string(),
            connector_type: ConnectorType::Http,
            config: json!({"url": "https://example.com"}),
        };
        assert!(validate_create_connector(&req).is_err());
    }

    #[test]
    fn test_validate_create_connector_empty_name() {
        let req = CreateConnectorRequest {
            id: None,
            name: "".to_string(),
            connector_type: ConnectorType::Http,
            config: json!({"url": "https://example.com"}),
        };
        assert!(validate_create_connector(&req).is_err());
    }

    #[test]
    fn test_validate_update_connector_with_name() {
        let req = UpdateConnectorRequest {
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
}
