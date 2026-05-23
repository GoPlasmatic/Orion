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
    // B1: collect all protocol-conditional missing-field errors in one
    // response (instead of failing on the first). Channel authors get
    // the full list of what to fix instead of one round-trip per issue.
    let protocol_details = collect_protocol_required_fields(req);
    if !protocol_details.is_empty() {
        return Err(OrionError::Validation {
            code: "VALIDATION_ERROR",
            message: format!(
                "Channel with protocol=\"{}\" is missing {} required field(s)",
                req.protocol,
                protocol_details.len()
            ),
            details: protocol_details,
        });
    }
    // B2: strict-validate the per-channel `config` blob at create time.
    // The channel registry stays tolerant at runtime (so an already-active
    // channel with a corrupt row doesn't crash engine reload), but new
    // creates fail fast with field-pathed errors so authors learn at the
    // CRUD boundary, not at first request.
    validate_channel_config_blob(&req.config)?;
    Ok(())
}

/// Per-protocol required-field check. Returns one `FieldError` per missing
/// field, all with `code = "REQUIRED_FOR_PROTOCOL"` so clients can
/// distinguish "this field is conditionally required" from a generic
/// missing-field error.
fn collect_protocol_required_fields(req: &CreateChannelRequest) -> Vec<crate::errors::FieldError> {
    use crate::errors::FieldError;
    let mut out = Vec::new();
    match req.protocol {
        ChannelProtocol::Rest | ChannelProtocol::Http => {
            if req.methods.as_ref().is_none_or(|m| m.is_empty()) {
                out.push(
                    FieldError::new(
                        "channel.methods",
                        "REQUIRED_FOR_PROTOCOL",
                        format!(
                            "REST/HTTP channels must specify at least one HTTP method (protocol=\"{}\")",
                            req.protocol
                        ),
                    )
                    .with_expected(serde_json::Value::String(
                        "non-empty array of method names".to_string(),
                    )),
                );
            }
            if req
                .route_pattern
                .as_ref()
                .is_none_or(|r| r.trim().is_empty())
            {
                out.push(
                    FieldError::new(
                        "channel.route_pattern",
                        "REQUIRED_FOR_PROTOCOL",
                        format!(
                            "REST/HTTP channels must specify a route_pattern (protocol=\"{}\")",
                            req.protocol
                        ),
                    )
                    .with_expected(serde_json::Value::String(
                        "URL path pattern (e.g. \"/orders/{id}\")".to_string(),
                    )),
                );
            }
        }
        ChannelProtocol::Kafka => {
            if req.topic.as_ref().is_none_or(|t| t.trim().is_empty()) {
                out.push(FieldError::new(
                    "channel.topic",
                    "REQUIRED_FOR_PROTOCOL",
                    "Kafka channels must specify a topic",
                ));
            }
        }
    }
    out
}

/// Strict-validate the channel `config` Value: parses to `ChannelConfig` to
/// catch shape errors and compiles every embedded JSONLogic expression
/// (`validation_logic`, `rate_limit.key_logic`) so typos surface here
/// rather than at engine reload (where they downgrade to warnings).
fn validate_channel_config_blob(config: &serde_json::Value) -> Result<(), OrionError> {
    // Empty object is the documented default for "no config" — skip parsing.
    if let Some(obj) = config.as_object()
        && obj.is_empty()
    {
        return Ok(());
    }
    let parsed: crate::channel::ChannelConfig =
        serde_json::from_value(config.clone()).map_err(|e| {
            OrionError::invalid_field(
                "channel.config",
                "INVALID",
                format!("channel.config does not match the ChannelConfig shape: {e}"),
            )
        })?;

    let dl = datalogic_rs::Engine::new();
    if let Some(ref logic) = parsed.validation_logic {
        dl.compile(logic).map_err(|e| {
            OrionError::invalid_field(
                "channel.config.validation_logic",
                "INVALID",
                format!("validation_logic is not a valid JSONLogic expression: {e}"),
            )
        })?;
    }
    if let Some(ref rl) = parsed.rate_limit
        && let Some(ref logic) = rl.key_logic
    {
        dl.compile(logic).map_err(|e| {
            OrionError::invalid_field(
                "channel.config.rate_limit.key_logic",
                "INVALID",
                format!("rate_limit.key_logic is not a valid JSONLogic expression: {e}"),
            )
        })?;
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
