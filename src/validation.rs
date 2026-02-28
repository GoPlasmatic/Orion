use crate::connector::{ConnectorConfig, VALID_CONNECTOR_TYPES};
use crate::errors::OrionError;
use crate::storage::repositories::connectors::{CreateConnectorRequest, UpdateConnectorRequest};
use crate::storage::repositories::rules::{CreateRuleRequest, UpdateRuleRequest};

const MAX_ID_LEN: usize = 128;
const MAX_NAME_LEN: usize = 255;
const MAX_DESCRIPTION_LEN: usize = 2048;
const MAX_CHANNEL_LEN: usize = 128;

/// Check if a string matches the identifier pattern:
/// starts with alphanumeric, then alphanumeric + dots/hyphens/underscores.
fn is_valid_identifier(s: &str) -> bool {
    let mut chars = s.chars();
    match chars.next() {
        Some(c) if c.is_ascii_alphanumeric() => {}
        _ => return false,
    }
    chars.all(|c| c.is_ascii_alphanumeric() || c == '.' || c == '-' || c == '_')
}

fn validate_id(id: &str) -> Result<(), OrionError> {
    if id.len() > MAX_ID_LEN {
        return Err(OrionError::BadRequest(format!(
            "ID exceeds maximum length of {MAX_ID_LEN} characters"
        )));
    }
    if !is_valid_identifier(id) {
        return Err(OrionError::BadRequest(
            "ID must start with an alphanumeric character and contain only alphanumeric characters, dots, hyphens, or underscores".to_string(),
        ));
    }
    Ok(())
}

fn validate_name(name: &str, field: &str) -> Result<(), OrionError> {
    if name.trim().is_empty() {
        return Err(OrionError::BadRequest(format!("{field} must not be empty")));
    }
    if name.len() > MAX_NAME_LEN {
        return Err(OrionError::BadRequest(format!(
            "{field} exceeds maximum length of {MAX_NAME_LEN} characters"
        )));
    }
    Ok(())
}

fn validate_description(desc: &str) -> Result<(), OrionError> {
    if desc.len() > MAX_DESCRIPTION_LEN {
        return Err(OrionError::BadRequest(format!(
            "Description exceeds maximum length of {MAX_DESCRIPTION_LEN} characters"
        )));
    }
    Ok(())
}

fn validate_channel(channel: &str) -> Result<(), OrionError> {
    if channel.trim().is_empty() {
        return Err(OrionError::BadRequest(
            "Channel must not be empty".to_string(),
        ));
    }
    if channel.len() > MAX_CHANNEL_LEN {
        return Err(OrionError::BadRequest(format!(
            "Channel exceeds maximum length of {MAX_CHANNEL_LEN} characters"
        )));
    }
    if !is_valid_identifier(channel) {
        return Err(OrionError::BadRequest(
            "Channel must start with an alphanumeric character and contain only alphanumeric characters, dots, hyphens, or underscores".to_string(),
        ));
    }
    Ok(())
}

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
    if let ConnectorConfig::Http(http_config) = &parsed {
        if !http_config.url.is_empty() {
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
    }

    Ok(())
}

pub fn validate_create_rule(req: &CreateRuleRequest) -> Result<(), OrionError> {
    if let Some(ref id) = req.rule_id {
        validate_id(id)?;
    }
    validate_name(&req.name, "Name")?;
    if let Some(ref desc) = req.description {
        validate_description(desc)?;
    }
    validate_channel(&req.channel)?;
    Ok(())
}

pub fn validate_update_rule(req: &UpdateRuleRequest) -> Result<(), OrionError> {
    if let Some(ref name) = req.name {
        validate_name(name, "Name")?;
    }
    if let Some(ref desc) = req.description {
        validate_description(desc)?;
    }
    if let Some(ref channel) = req.channel {
        validate_channel(channel)?;
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
    fn test_valid_id() {
        assert!(validate_id("my-rule-1").is_ok());
        assert!(validate_id("rule.v2").is_ok());
        assert!(validate_id("A123_test").is_ok());
    }

    #[test]
    fn test_invalid_id_chars() {
        assert!(validate_id("").is_err());
        assert!(validate_id("-starts-with-dash").is_err());
        assert!(validate_id(".starts-with-dot").is_err());
        assert!(validate_id("has spaces").is_err());
        assert!(validate_id("has/slash").is_err());
    }

    #[test]
    fn test_id_too_long() {
        let long_id = "a".repeat(MAX_ID_LEN + 1);
        assert!(validate_id(&long_id).is_err());
    }

    #[test]
    fn test_valid_name() {
        assert!(validate_name("My Rule", "Name").is_ok());
    }

    #[test]
    fn test_empty_name() {
        assert!(validate_name("", "Name").is_err());
        assert!(validate_name("   ", "Name").is_err());
    }

    #[test]
    fn test_name_too_long() {
        let long_name = "a".repeat(MAX_NAME_LEN + 1);
        assert!(validate_name(&long_name, "Name").is_err());
    }

    #[test]
    fn test_description_too_long() {
        let long_desc = "a".repeat(MAX_DESCRIPTION_LEN + 1);
        assert!(validate_description(&long_desc).is_err());
    }

    #[test]
    fn test_valid_channel() {
        assert!(validate_channel("orders").is_ok());
        assert!(validate_channel("my-channel.v2").is_ok());
    }

    #[test]
    fn test_invalid_channel() {
        assert!(validate_channel("").is_err());
        assert!(validate_channel("   ").is_err());
        assert!(validate_channel("has spaces").is_err());
    }

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
    fn test_channel_too_long() {
        let long_channel = "a".repeat(MAX_CHANNEL_LEN + 1);
        assert!(validate_channel(&long_channel).is_err());
    }

    #[test]
    fn test_description_valid() {
        assert!(validate_description("A short description").is_ok());
        assert!(validate_description("").is_ok());
    }

    #[test]
    fn test_validate_create_rule_full() {
        let req = CreateRuleRequest {
            rule_id: Some("my-rule-1".to_string()),
            name: "Test Rule".to_string(),
            description: Some("A test rule".to_string()),
            channel: "orders".to_string(),
            priority: 10,
            condition: json!(true),
            tasks: json!([]),
            tags: vec!["tag1".to_string()],
            continue_on_error: false,
        };
        assert!(validate_create_rule(&req).is_ok());
    }

    #[test]
    fn test_validate_create_rule_invalid_id() {
        let req = CreateRuleRequest {
            rule_id: Some("bad id with spaces".to_string()),
            name: "Test Rule".to_string(),
            description: None,
            channel: "orders".to_string(),
            priority: 0,
            condition: json!(true),
            tasks: json!([]),
            tags: vec![],
            continue_on_error: false,
        };
        assert!(validate_create_rule(&req).is_err());
    }

    #[test]
    fn test_validate_create_rule_long_description() {
        let req = CreateRuleRequest {
            rule_id: None,
            name: "Test Rule".to_string(),
            description: Some("d".repeat(MAX_DESCRIPTION_LEN + 1)),
            channel: "orders".to_string(),
            priority: 0,
            condition: json!(true),
            tasks: json!([]),
            tags: vec![],
            continue_on_error: false,
        };
        assert!(validate_create_rule(&req).is_err());
    }

    #[test]
    fn test_validate_update_rule_all_fields() {
        let req = UpdateRuleRequest {
            name: Some("Updated Name".to_string()),
            description: Some("Updated desc".to_string()),
            channel: Some("updated-ch".to_string()),
            priority: Some(5),
            condition: None,
            tasks: None,
            tags: None,
            continue_on_error: None,
        };
        assert!(validate_update_rule(&req).is_ok());
    }

    #[test]
    fn test_validate_update_rule_invalid_name() {
        let req = UpdateRuleRequest {
            name: Some("".to_string()),
            description: None,
            channel: None,
            priority: None,
            condition: None,
            tasks: None,
            tags: None,
            continue_on_error: None,
        };
        assert!(validate_update_rule(&req).is_err());
    }

    #[test]
    fn test_validate_update_rule_invalid_description() {
        let req = UpdateRuleRequest {
            name: None,
            description: Some("x".repeat(MAX_DESCRIPTION_LEN + 1)),
            channel: None,
            priority: None,
            condition: None,
            tasks: None,
            tags: None,
            continue_on_error: None,
        };
        assert!(validate_update_rule(&req).is_err());
    }

    #[test]
    fn test_validate_update_rule_invalid_channel() {
        let req = UpdateRuleRequest {
            name: None,
            description: None,
            channel: Some("bad channel!".to_string()),
            priority: None,
            condition: None,
            tasks: None,
            tags: None,
            continue_on_error: None,
        };
        assert!(validate_update_rule(&req).is_err());
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
}
