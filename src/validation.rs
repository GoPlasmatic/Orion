use std::net::IpAddr;

use crate::connector::{ConnectorConfig, VALID_CONNECTOR_TYPES};
use crate::errors::OrionError;
use crate::storage::models::{
    CHANNEL_PROTOCOL_HTTP, CHANNEL_PROTOCOL_KAFKA, CHANNEL_PROTOCOL_REST,
};
use crate::storage::repositories::channels::CreateChannelRequest;
use crate::storage::repositories::connectors::{CreateConnectorRequest, UpdateConnectorRequest};
use crate::storage::repositories::workflows::{CreateWorkflowRequest, UpdateWorkflowRequest};

const MAX_ID_LEN: usize = 128;
const MAX_NAME_LEN: usize = 255;
const MAX_DESCRIPTION_LEN: usize = 2048;

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

/// Check if an IP address is private, loopback, link-local, or otherwise internal.
pub fn is_private_ip(ip: &IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => {
            v4.is_loopback()               // 127.0.0.0/8
            || v4.is_private()              // 10/8, 172.16/12, 192.168/16
            || v4.is_link_local()           // 169.254.0.0/16
            || v4.is_unspecified()          // 0.0.0.0
            || v4.is_broadcast()            // 255.255.255.255
            || v4.octets()[0] == 100 && (v4.octets()[1] & 0xC0) == 64 // 100.64.0.0/10 (CGNAT)
        }
        IpAddr::V6(v6) => {
            v6.is_loopback()                // ::1
            || v6.is_unspecified()          // ::
            // IPv4-mapped ::ffff:x.x.x.x — check inner v4
            || v6.to_ipv4_mapped().is_some_and(|v4| is_private_ip(&IpAddr::V4(v4)))
        }
    }
}

/// Validate that a URL does not target private/internal IP addresses (SSRF protection).
/// Resolves the hostname and checks all resolved addresses.
pub async fn validate_url_not_private(url: &str) -> Result<(), String> {
    let parsed = url::Url::parse(url).map_err(|e| format!("Invalid URL '{}': {}", url, e))?;

    let host = match parsed.host_str() {
        Some(h) => h,
        None => return Err(format!("URL '{}' has no host", url)),
    };

    // Direct IP address check
    if let Ok(ip) = host.parse::<IpAddr>() {
        if is_private_ip(&ip) {
            return Err(format!(
                "URL '{}' targets private/internal IP address {}",
                url, ip
            ));
        }
        return Ok(());
    }

    // DNS resolution check
    let port = parsed.port_or_known_default().unwrap_or(80);
    let addr = format!("{}:{}", host, port);
    match tokio::net::lookup_host(&addr).await {
        Ok(addrs) => {
            for socket_addr in addrs {
                if is_private_ip(&socket_addr.ip()) {
                    return Err(format!(
                        "URL '{}' resolves to private/internal IP address {}",
                        url,
                        socket_addr.ip()
                    ));
                }
            }
        }
        Err(_) => {
            // DNS resolution failure is not an SSRF issue — let the HTTP client handle it
        }
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

    Ok(())
}

pub fn validate_create_workflow(req: &CreateWorkflowRequest) -> Result<(), OrionError> {
    if let Some(ref id) = req.workflow_id {
        validate_id(id)?;
    }
    validate_name(&req.name, "Name")?;
    if let Some(ref desc) = req.description {
        validate_description(desc)?;
    }
    Ok(())
}

pub fn validate_update_workflow(req: &UpdateWorkflowRequest) -> Result<(), OrionError> {
    if let Some(ref name) = req.name {
        validate_name(name, "Name")?;
    }
    if let Some(ref desc) = req.description {
        validate_description(desc)?;
    }
    Ok(())
}

pub fn validate_workflow_id(id: &str) -> Result<(), OrionError> {
    validate_id(id)
}

pub fn validate_create_channel(req: &CreateChannelRequest) -> Result<(), OrionError> {
    if let Some(ref id) = req.channel_id {
        validate_id(id)?;
    }
    validate_name(&req.name, "Name")?;
    if let Some(ref desc) = req.description {
        validate_description(desc)?;
    }
    // REST/HTTP channels need methods + route_pattern
    if req.protocol == CHANNEL_PROTOCOL_REST || req.protocol == CHANNEL_PROTOCOL_HTTP {
        if req.methods.as_ref().is_none_or(|m| m.is_empty()) {
            return Err(OrionError::BadRequest(
                "REST/HTTP channels must specify at least one HTTP method".to_string(),
            ));
        }
        if req
            .route_pattern
            .as_ref()
            .is_none_or(|r| r.trim().is_empty())
        {
            return Err(OrionError::BadRequest(
                "REST/HTTP channels must specify a route_pattern".to_string(),
            ));
        }
    }
    // Kafka channels need a topic
    if req.protocol == CHANNEL_PROTOCOL_KAFKA
        && req.topic.as_ref().is_none_or(|t| t.trim().is_empty())
    {
        return Err(OrionError::BadRequest(
            "Kafka channels must specify a topic".to_string(),
        ));
    }
    Ok(())
}

pub fn validate_channel_id(id: &str) -> Result<(), OrionError> {
    validate_id(id)
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
        assert!(validate_id("my-workflow-1").is_ok());
        assert!(validate_id("workflow.v2").is_ok());
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
        assert!(validate_name("My Workflow", "Name").is_ok());
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
        assert!(validate_channel_id("orders").is_ok());
        assert!(validate_channel_id("my-channel.v2").is_ok());
    }

    #[test]
    fn test_invalid_channel() {
        assert!(validate_channel_id("").is_err());
        assert!(validate_channel_id("   ").is_err());
        assert!(validate_channel_id("has spaces").is_err());
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
        let long_channel = "a".repeat(MAX_ID_LEN + 1);
        assert!(validate_channel_id(&long_channel).is_err());
    }

    #[test]
    fn test_description_valid() {
        assert!(validate_description("A short description").is_ok());
        assert!(validate_description("").is_ok());
    }

    #[test]
    fn test_validate_create_workflow_full() {
        let req = CreateWorkflowRequest {
            workflow_id: Some("my-workflow-1".to_string()),
            name: "Test Workflow".to_string(),
            description: Some("A test workflow".to_string()),
            priority: 10,
            condition: json!(true),
            tasks: json!([]),
            tags: vec!["tag1".to_string()],
            continue_on_error: false,
        };
        assert!(validate_create_workflow(&req).is_ok());
    }

    #[test]
    fn test_validate_create_workflow_invalid_id() {
        let req = CreateWorkflowRequest {
            workflow_id: Some("bad id with spaces".to_string()),
            name: "Test Workflow".to_string(),
            description: None,
            priority: 0,
            condition: json!(true),
            tasks: json!([]),
            tags: vec![],
            continue_on_error: false,
        };
        assert!(validate_create_workflow(&req).is_err());
    }

    #[test]
    fn test_validate_create_workflow_long_description() {
        let req = CreateWorkflowRequest {
            workflow_id: None,
            name: "Test Workflow".to_string(),
            description: Some("d".repeat(MAX_DESCRIPTION_LEN + 1)),
            priority: 0,
            condition: json!(true),
            tasks: json!([]),
            tags: vec![],
            continue_on_error: false,
        };
        assert!(validate_create_workflow(&req).is_err());
    }

    #[test]
    fn test_validate_update_workflow_all_fields() {
        let req = UpdateWorkflowRequest {
            name: Some("Updated Name".to_string()),
            description: Some("Updated desc".to_string()),
            priority: Some(5),
            condition: None,
            tasks: None,
            tags: None,
            continue_on_error: None,
        };
        assert!(validate_update_workflow(&req).is_ok());
    }

    #[test]
    fn test_validate_update_workflow_invalid_name() {
        let req = UpdateWorkflowRequest {
            name: Some("".to_string()),
            description: None,
            priority: None,
            condition: None,
            tasks: None,
            tags: None,
            continue_on_error: None,
        };
        assert!(validate_update_workflow(&req).is_err());
    }

    #[test]
    fn test_validate_update_workflow_invalid_description() {
        let req = UpdateWorkflowRequest {
            name: None,
            description: Some("x".repeat(MAX_DESCRIPTION_LEN + 1)),
            priority: None,
            condition: None,
            tasks: None,
            tags: None,
            continue_on_error: None,
        };
        assert!(validate_update_workflow(&req).is_err());
    }

    #[test]
    fn test_validate_create_channel_sync_valid() {
        let req = CreateChannelRequest {
            channel_id: Some("orders-sync".to_string()),
            name: "Orders Sync".to_string(),
            description: None,
            channel_type: "sync".to_string(),
            protocol: "rest".to_string(),
            methods: Some(vec!["POST".to_string()]),
            route_pattern: Some("/orders".to_string()),
            topic: None,
            consumer_group: None,
            transport_config: json!({}),
            workflow_id: None,
            config: json!({}),
            priority: 0,
        };
        assert!(validate_create_channel(&req).is_ok());
    }

    #[test]
    fn test_validate_create_channel_sync_missing_methods() {
        let req = CreateChannelRequest {
            channel_id: None,
            name: "Bad Sync".to_string(),
            description: None,
            channel_type: "sync".to_string(),
            protocol: "rest".to_string(),
            methods: None,
            route_pattern: Some("/orders".to_string()),
            topic: None,
            consumer_group: None,
            transport_config: json!({}),
            workflow_id: None,
            config: json!({}),
            priority: 0,
        };
        assert!(validate_create_channel(&req).is_err());
    }

    #[test]
    fn test_validate_create_channel_sync_missing_route() {
        let req = CreateChannelRequest {
            channel_id: None,
            name: "Bad Sync".to_string(),
            description: None,
            channel_type: "sync".to_string(),
            protocol: "rest".to_string(),
            methods: Some(vec!["POST".to_string()]),
            route_pattern: None,
            topic: None,
            consumer_group: None,
            transport_config: json!({}),
            workflow_id: None,
            config: json!({}),
            priority: 0,
        };
        assert!(validate_create_channel(&req).is_err());
    }

    #[test]
    fn test_validate_create_channel_async_valid() {
        let req = CreateChannelRequest {
            channel_id: None,
            name: "Orders Async".to_string(),
            description: None,
            channel_type: "async".to_string(),
            protocol: "kafka".to_string(),
            methods: None,
            route_pattern: None,
            topic: Some("orders-topic".to_string()),
            consumer_group: None,
            transport_config: json!({}),
            workflow_id: None,
            config: json!({}),
            priority: 0,
        };
        assert!(validate_create_channel(&req).is_ok());
    }

    #[test]
    fn test_validate_create_channel_async_missing_topic() {
        let req = CreateChannelRequest {
            channel_id: None,
            name: "Bad Async".to_string(),
            description: None,
            channel_type: "async".to_string(),
            protocol: "kafka".to_string(),
            methods: None,
            route_pattern: None,
            topic: None,
            consumer_group: None,
            transport_config: json!({}),
            workflow_id: None,
            config: json!({}),
            priority: 0,
        };
        assert!(validate_create_channel(&req).is_err());
    }

    #[test]
    fn test_validate_create_channel_kafka_valid() {
        let req = CreateChannelRequest {
            channel_id: None,
            name: "Kafka Channel".to_string(),
            description: None,
            channel_type: "async".to_string(),
            protocol: "kafka".to_string(),
            methods: None,
            route_pattern: None,
            topic: Some("kafka-topic".to_string()),
            consumer_group: Some("my-group".to_string()),
            transport_config: json!({}),
            workflow_id: None,
            config: json!({}),
            priority: 0,
        };
        assert!(validate_create_channel(&req).is_ok());
    }

    #[test]
    fn test_validate_channel_id() {
        assert!(validate_channel_id("my-channel-1").is_ok());
        assert!(validate_channel_id("bad id!").is_err());
    }

    #[test]
    fn test_validate_workflow_id() {
        assert!(validate_workflow_id("my-workflow-1").is_ok());
        assert!(validate_workflow_id("bad id!").is_err());
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

    // --- SSRF protection tests ---

    #[test]
    fn test_is_private_ip_loopback() {
        assert!(is_private_ip(&"127.0.0.1".parse().unwrap()));
        assert!(is_private_ip(&"127.0.0.2".parse().unwrap()));
        assert!(is_private_ip(&"::1".parse().unwrap()));
    }

    #[test]
    fn test_is_private_ip_rfc1918() {
        assert!(is_private_ip(&"10.0.0.1".parse().unwrap()));
        assert!(is_private_ip(&"10.255.255.255".parse().unwrap()));
        assert!(is_private_ip(&"172.16.0.1".parse().unwrap()));
        assert!(is_private_ip(&"172.31.255.255".parse().unwrap()));
        assert!(is_private_ip(&"192.168.0.1".parse().unwrap()));
        assert!(is_private_ip(&"192.168.255.255".parse().unwrap()));
    }

    #[test]
    fn test_is_private_ip_link_local() {
        assert!(is_private_ip(&"169.254.0.1".parse().unwrap()));
        assert!(is_private_ip(&"169.254.169.254".parse().unwrap())); // Cloud metadata
    }

    #[test]
    fn test_is_private_ip_cgnat() {
        assert!(is_private_ip(&"100.64.0.1".parse().unwrap()));
        assert!(is_private_ip(&"100.127.255.255".parse().unwrap()));
    }

    #[test]
    fn test_is_private_ip_public() {
        assert!(!is_private_ip(&"8.8.8.8".parse().unwrap()));
        assert!(!is_private_ip(&"1.1.1.1".parse().unwrap()));
        assert!(!is_private_ip(&"203.0.113.1".parse().unwrap()));
    }

    #[test]
    fn test_is_private_ip_v4_mapped_v6() {
        // ::ffff:127.0.0.1
        assert!(is_private_ip(&"::ffff:127.0.0.1".parse().unwrap()));
        // ::ffff:10.0.0.1
        assert!(is_private_ip(&"::ffff:10.0.0.1".parse().unwrap()));
        // ::ffff:8.8.8.8 (public)
        assert!(!is_private_ip(&"::ffff:8.8.8.8".parse().unwrap()));
    }

    #[tokio::test]
    async fn test_validate_url_not_private_direct_ip() {
        assert!(
            validate_url_not_private("http://127.0.0.1/api")
                .await
                .is_err()
        );
        assert!(
            validate_url_not_private("http://10.0.0.1:8080/api")
                .await
                .is_err()
        );
        assert!(
            validate_url_not_private("http://192.168.1.1/api")
                .await
                .is_err()
        );
        assert!(
            validate_url_not_private("http://169.254.169.254/latest/meta-data")
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn test_validate_url_not_private_no_host() {
        assert!(
            validate_url_not_private("data:text/plain,hello")
                .await
                .is_err()
        );
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
