use crate::connector::{ConnectorConfig, VALID_CONNECTOR_TYPES};
use crate::errors::OrionError;
use crate::storage::repositories::connectors::{CreateConnectorRequest, UpdateConnectorRequest};

use super::common::{validate_id, validate_name};

fn validate_connector_type(ct: &str) -> Result<(), OrionError> {
    if !VALID_CONNECTOR_TYPES.contains(&ct) {
        return Err(OrionError::BadRequest(format!(
            "Invalid connector type '{}'. Must be one of: {}",
            ct,
            VALID_CONNECTOR_TYPES.join(", ")
        )));
    }
    Ok(())
}

fn validate_connector_config(
    connector_type: &str,
    config: &serde_json::Value,
) -> Result<(), OrionError> {
    // Inject the type field so we can deserialize as the tagged enum
    let mut config_with_type = config.clone();
    if let Some(obj) = config_with_type.as_object_mut() {
        obj.insert(
            "type".to_string(),
            serde_json::Value::String(connector_type.to_string()),
        );
    } else {
        return Err(OrionError::BadRequest(
            "Connector config must be a JSON object".to_string(),
        ));
    }

    let parsed: ConnectorConfig = serde_json::from_value(config_with_type).map_err(|e| {
        OrionError::BadRequest(format!(
            "Invalid connector config for type '{connector_type}': {e}"
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
    validate_connector_type(&req.connector_type)?;
    validate_connector_config(&req.connector_type, &req.config)?;
    Ok(())
}

pub fn validate_update_connector(req: &UpdateConnectorRequest) -> Result<(), OrionError> {
    if let Some(ref name) = req.name {
        validate_name(name, "Name")?;
    }
    if let Some(ref ct) = req.connector_type {
        validate_connector_type(ct)?;
    }
    // If both type and config are provided, validate config against the new type
    if let Some(ref ct) = req.connector_type
        && let Some(ref config) = req.config
    {
        validate_connector_config(ct, config)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn test_connector_type_valid() {
        assert!(validate_connector_type("http").is_ok());
        assert!(validate_connector_type("kafka").is_ok());
    }

    #[test]
    fn test_connector_type_invalid() {
        assert!(validate_connector_type("grpc").is_err());
        assert!(validate_connector_type("").is_err());
    }

    #[test]
    fn test_connector_config_http_valid() {
        let config = json!({
            "url": "https://example.com/api",
            "method": "POST"
        });
        assert!(validate_connector_config("http", &config).is_ok());
    }

    #[test]
    fn test_connector_config_http_invalid_scheme() {
        let config = json!({
            "url": "ftp://example.com/api",
            "method": "POST"
        });
        assert!(validate_connector_config("http", &config).is_err());
    }

    #[test]
    fn test_connector_config_invalid_structure() {
        let config = json!("not an object");
        assert!(validate_connector_config("http", &config).is_err());
    }

    #[test]
    fn test_connector_config_http_empty_url() {
        let config = json!({"url": ""});
        // Empty URL should be fine (passes URL validation skip)
        assert!(validate_connector_config("http", &config).is_ok());
    }

    #[test]
    fn test_connector_config_http_invalid_url() {
        let config = json!({"url": "not a valid url"});
        assert!(validate_connector_config("http", &config).is_err());
    }

    #[test]
    fn test_validate_create_connector_with_id() {
        let req = CreateConnectorRequest {
            id: Some("my-conn-1".to_string()),
            name: "My Connector".to_string(),
            connector_type: "http".to_string(),
            config: json!({"url": "https://example.com"}),
        };
        assert!(validate_create_connector(&req).is_ok());
    }

    #[test]
    fn test_validate_create_connector_invalid_id() {
        let req = CreateConnectorRequest {
            id: Some("bad id!".to_string()),
            name: "My Connector".to_string(),
            connector_type: "http".to_string(),
            config: json!({"url": "https://example.com"}),
        };
        assert!(validate_create_connector(&req).is_err());
    }

    #[test]
    fn test_validate_create_connector_empty_name() {
        let req = CreateConnectorRequest {
            id: None,
            name: "".to_string(),
            connector_type: "http".to_string(),
            config: json!({"url": "https://example.com"}),
        };
        assert!(validate_create_connector(&req).is_err());
    }

    #[test]
    fn test_validate_create_connector_invalid_type() {
        let req = CreateConnectorRequest {
            id: None,
            name: "Test".to_string(),
            connector_type: "grpc".to_string(),
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
            connector_type: Some("http".to_string()),
            config: None,
            enabled: None,
        };
        assert!(validate_update_connector(&req).is_ok());
    }

    #[test]
    fn test_validate_update_connector_invalid_type() {
        let req = UpdateConnectorRequest {
            name: None,
            connector_type: Some("invalid".to_string()),
            config: None,
            enabled: None,
        };
        assert!(validate_update_connector(&req).is_err());
    }

    #[test]
    fn test_validate_update_connector_type_and_config() {
        let req = UpdateConnectorRequest {
            name: None,
            connector_type: Some("http".to_string()),
            config: Some(json!({"url": "https://example.com"})),
            enabled: None,
        };
        assert!(validate_update_connector(&req).is_ok());
    }

    #[test]
    fn test_validate_update_connector_type_and_invalid_config() {
        let req = UpdateConnectorRequest {
            name: None,
            connector_type: Some("http".to_string()),
            config: Some(json!("not an object")),
            enabled: None,
        };
        assert!(validate_update_connector(&req).is_err());
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
}
