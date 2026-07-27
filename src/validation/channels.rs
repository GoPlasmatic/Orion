use crate::errors::OrionError;
use crate::storage::models::{Channel, ChannelProtocol};
use crate::storage::repositories::channels::{CreateChannelRequest, UpdateChannelRequest};

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
    check_protocol_required_fields(&ProtocolFields {
        protocol: req.protocol,
        methods: req.methods.as_deref(),
        route_pattern: req.route_pattern.as_deref(),
        topic: req.topic.as_deref(),
    })?;
    // B2: strict-validate the per-channel `config` blob at create time.
    // The channel registry stays tolerant at runtime (so an already-active
    // channel with a corrupt row doesn't crash engine reload), but new
    // creates fail fast with field-pathed errors so authors learn at the
    // CRUD boundary, not at first request.
    validate_channel_config_blob(&req.config)?;
    Ok(())
}

/// R3: mirror the create-time checks on `PUT /channels/{id}`, which
/// previously ran no validation at all. `UpdateChannelRequest` carries no
/// protocol (it is immutable across versions), so the protocol-conditional
/// checks run against the merged (stored draft ⊕ request) view: a field
/// omitted from the request keeps its stored value, while an explicit `""`
/// or `[]` counts as emptying it and is rejected when the stored protocol
/// requires it.
pub fn validate_update_channel(
    stored: &Channel,
    req: &UpdateChannelRequest,
) -> Result<(), OrionError> {
    if let Some(ref name) = req.name {
        validate_name(name, "Name").map_err(|e| remap_to_field(e, "channel.name"))?;
    }
    if let Some(ref desc) = req.description {
        validate_description(desc).map_err(|e| remap_to_field(e, "channel.description"))?;
    }
    // Stored `methods` is a JSON-encoded array column; a stored row that
    // fails to parse contributes no methods, so the request must supply them.
    let stored_methods: Option<Vec<String>> = stored
        .methods
        .as_deref()
        .and_then(|m| serde_json::from_str(m).ok());
    // A stored protocol outside the known set (corrupt row) skips the
    // protocol-conditional checks rather than blocking unrelated updates.
    let stored_protocol: Option<ChannelProtocol> =
        serde_json::from_value(serde_json::Value::String(stored.protocol.clone())).ok();
    if let Some(protocol) = stored_protocol {
        check_protocol_required_fields(&ProtocolFields {
            protocol,
            methods: req.methods.as_deref().or(stored_methods.as_deref()),
            route_pattern: req
                .route_pattern
                .as_deref()
                .or(stored.route_pattern.as_deref()),
            topic: req.topic.as_deref().or(stored.topic.as_deref()),
        })?;
    }
    if let Some(ref config) = req.config {
        validate_channel_config_blob(config)?;
    }
    Ok(())
}

/// The protocol-conditional fields of a channel, as seen by validation —
/// the request itself at create time, the merged stored ⊕ request view at
/// update time.
struct ProtocolFields<'a> {
    protocol: ChannelProtocol,
    methods: Option<&'a [String]>,
    route_pattern: Option<&'a str>,
    topic: Option<&'a str>,
}

/// Per-protocol required-field check. Emits one `FieldError` per missing
/// field, all with `code = "REQUIRED_FOR_PROTOCOL"` so clients can
/// distinguish "this field is conditionally required" from a generic
/// missing-field error.
fn check_protocol_required_fields(fields: &ProtocolFields) -> Result<(), OrionError> {
    use crate::errors::FieldError;
    let mut out = Vec::new();
    match fields.protocol {
        ChannelProtocol::Rest | ChannelProtocol::Http => {
            if fields.methods.is_none_or(|m| m.is_empty()) {
                out.push(
                    FieldError::new(
                        "channel.methods",
                        "REQUIRED_FOR_PROTOCOL",
                        format!(
                            "REST/HTTP channels must specify at least one HTTP method (protocol=\"{}\")",
                            fields.protocol
                        ),
                    )
                    .with_expected(serde_json::Value::String(
                        "non-empty array of method names".to_string(),
                    )),
                );
            }
            if fields.route_pattern.is_none_or(|r| r.trim().is_empty()) {
                out.push(
                    FieldError::new(
                        "channel.route_pattern",
                        "REQUIRED_FOR_PROTOCOL",
                        format!(
                            "REST/HTTP channels must specify a route_pattern (protocol=\"{}\")",
                            fields.protocol
                        ),
                    )
                    .with_expected(serde_json::Value::String(
                        "URL path pattern (e.g. \"/orders/{id}\")".to_string(),
                    )),
                );
            }
        }
        ChannelProtocol::Kafka => {
            if fields.topic.is_none_or(|t| t.trim().is_empty()) {
                out.push(FieldError::new(
                    "channel.topic",
                    "REQUIRED_FOR_PROTOCOL",
                    "Kafka channels must specify a topic",
                ));
            }
        }
    }
    if out.is_empty() {
        return Ok(());
    }
    Err(OrionError::Validation {
        code: "VALIDATION_ERROR",
        message: format!(
            "Channel with protocol=\"{}\" is missing {} required field(s)",
            fields.protocol,
            out.len()
        ),
        details: out,
    })
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

    // -- validate_update_channel (R3) --

    fn stored_rest_channel() -> Channel {
        Channel {
            channel_id: "orders".to_string(),
            version: 1,
            name: "Orders".to_string(),
            description: None,
            channel_type: "sync".to_string(),
            protocol: "rest".to_string(),
            methods: Some("[\"POST\"]".to_string()),
            route_pattern: Some("/orders".to_string()),
            topic: None,
            consumer_group: None,
            transport_config_json: "{}".to_string(),
            workflow_id: None,
            config_json: "{}".to_string(),
            status: "draft".to_string(),
            priority: 0,
            created_at: chrono::Utc::now().naive_utc(),
            updated_at: chrono::Utc::now().naive_utc(),
        }
    }

    fn empty_update() -> UpdateChannelRequest {
        UpdateChannelRequest {
            name: None,
            description: None,
            methods: None,
            route_pattern: None,
            topic: None,
            consumer_group: None,
            transport_config: None,
            workflow_id: None,
            config: None,
            priority: None,
        }
    }

    #[test]
    fn test_update_omitted_fields_keep_stored_values() {
        let stored = stored_rest_channel();
        let req = UpdateChannelRequest {
            name: Some("New Name".to_string()),
            ..empty_update()
        };
        assert!(validate_update_channel(&stored, &req).is_ok());
    }

    #[test]
    fn test_update_emptying_route_pattern_rejected() {
        let stored = stored_rest_channel();
        let req = UpdateChannelRequest {
            route_pattern: Some("".to_string()),
            ..empty_update()
        };
        assert!(validate_update_channel(&stored, &req).is_err());
    }

    #[test]
    fn test_update_emptying_methods_rejected() {
        let stored = stored_rest_channel();
        let req = UpdateChannelRequest {
            methods: Some(vec![]),
            ..empty_update()
        };
        assert!(validate_update_channel(&stored, &req).is_err());
    }

    #[test]
    fn test_update_emptying_topic_rejected_for_kafka() {
        let stored = Channel {
            protocol: "kafka".to_string(),
            methods: None,
            route_pattern: None,
            topic: Some("orders-topic".to_string()),
            ..stored_rest_channel()
        };
        let req = UpdateChannelRequest {
            topic: Some("  ".to_string()),
            ..empty_update()
        };
        assert!(validate_update_channel(&stored, &req).is_err());
        // Omitting topic keeps the stored one
        assert!(validate_update_channel(&stored, &empty_update()).is_ok());
    }

    #[test]
    fn test_update_malformed_config_rejected() {
        let stored = stored_rest_channel();
        let req = UpdateChannelRequest {
            config: Some(json!({"rate_limit": 42})),
            ..empty_update()
        };
        assert!(validate_update_channel(&stored, &req).is_err());
    }

    #[test]
    fn test_update_invalid_name_rejected() {
        let stored = stored_rest_channel();
        let req = UpdateChannelRequest {
            name: Some("   ".to_string()),
            ..empty_update()
        };
        assert!(validate_update_channel(&stored, &req).is_err());
    }

    #[test]
    fn test_update_replacing_protocol_fields_with_valid_values_accepted() {
        let stored = stored_rest_channel();
        let req = UpdateChannelRequest {
            methods: Some(vec!["GET".to_string(), "POST".to_string()]),
            route_pattern: Some("/orders/{id}".to_string()),
            config: Some(json!({"timeout_ms": 5000})),
            ..empty_update()
        };
        assert!(validate_update_channel(&stored, &req).is_ok());
    }
}
