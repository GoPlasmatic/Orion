use crate::errors::OrionError;
use crate::storage::models::ChannelProtocol;
use crate::storage::repositories::channels::CreateChannelRequest;

use super::common::{validate_description, validate_id, validate_name};

pub fn validate_create_channel(req: &CreateChannelRequest) -> Result<(), OrionError> {
    if let Some(ref id) = req.channel_id {
        validate_id(id).map_err(|e| remap_to_field(e, "channel.channel_id"))?;
    }
    validate_name(&req.name, "Name").map_err(|e| remap_to_field(e, "channel.name"))?;
    if let Some(ref desc) = req.description {
        validate_description(desc).map_err(|e| remap_to_field(e, "channel.description"))?;
    }
    // REST/HTTP channels need methods + route_pattern
    if matches!(req.protocol, ChannelProtocol::Rest | ChannelProtocol::Http) {
        if req.methods.as_ref().is_none_or(|m| m.is_empty()) {
            return Err(OrionError::invalid_field(
                "channel.methods",
                "REQUIRED",
                format!(
                    "REST/HTTP channels must specify at least one HTTP method (protocol=\"{}\")",
                    req.protocol
                ),
            ));
        }
        if req
            .route_pattern
            .as_ref()
            .is_none_or(|r| r.trim().is_empty())
        {
            return Err(OrionError::invalid_field(
                "channel.route_pattern",
                "REQUIRED",
                format!(
                    "REST/HTTP channels must specify a route_pattern (protocol=\"{}\")",
                    req.protocol
                ),
            ));
        }
    }
    // Kafka channels need a topic
    if req.protocol == ChannelProtocol::Kafka
        && req.topic.as_ref().is_none_or(|t| t.trim().is_empty())
    {
        return Err(OrionError::invalid_field(
            "channel.topic",
            "REQUIRED",
            "Kafka channels must specify a topic",
        ));
    }
    Ok(())
}

/// Promote a `BadRequest` returned by a shared `common::*` validator to a
/// `Validation` error with the caller's field path. Other variants pass through.
fn remap_to_field(err: OrionError, path: &'static str) -> OrionError {
    match err {
        OrionError::BadRequest(msg) => OrionError::invalid_field(path, "INVALID", msg),
        other => other,
    }
}

pub fn validate_channel_id(id: &str) -> Result<(), OrionError> {
    validate_id(id)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::validation::common::MAX_ID_LEN;
    use serde_json::json;

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
    fn test_channel_too_long() {
        let long_channel = "a".repeat(MAX_ID_LEN + 1);
        assert!(validate_channel_id(&long_channel).is_err());
    }

    #[test]
    fn test_validate_create_channel_sync_valid() {
        let req = CreateChannelRequest {
            channel_id: Some("orders-sync".to_string()),
            name: "Orders Sync".to_string(),
            description: None,
            channel_type: crate::storage::models::ChannelType::Sync,
            protocol: ChannelProtocol::Rest,
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
            channel_type: crate::storage::models::ChannelType::Sync,
            protocol: ChannelProtocol::Rest,
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
            channel_type: crate::storage::models::ChannelType::Sync,
            protocol: ChannelProtocol::Rest,
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
            channel_type: crate::storage::models::ChannelType::Async,
            protocol: ChannelProtocol::Kafka,
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
            channel_type: crate::storage::models::ChannelType::Async,
            protocol: ChannelProtocol::Kafka,
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
            channel_type: crate::storage::models::ChannelType::Async,
            protocol: ChannelProtocol::Kafka,
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
}
