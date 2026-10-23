//! Native Rust channel driver interface and registry (`V2-11.1`).
//!
//! In Orion v2, channels remain 100% Native Rust ingress and scheduler drivers
//! implementing [`ChannelDriver`]:
//! - **Community Edition** (`ChannelDriverRegistry::community()`) registers the
//!   foundational drivers: `"rest"`, `"http"`, `"cron"`, and `"source"`.
//! - **Enterprise Edition** extends the registry at its composition root with
//!   native Rust drivers such as `"graphql"`, `"kafka"`, and `"sse"` without
//!   requiring a closed `ChannelProtocol` enum or per-protocol match arms in core.

use std::collections::BTreeMap;
use std::sync::{Arc, RwLock};

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::errors::{FieldError, OrionError};

/// Capabilities declared by a native Rust [`ChannelDriver`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChannelDriverCapabilities {
    pub supports_sync: bool,
    pub supports_async: bool,
    pub registers_http_routes: bool,
    pub supports_oauth2_login: bool,
}

/// Protocol-conditional fields evaluated during channel validation (`V2-11.1`).
#[derive(Debug, Clone)]
pub struct ChannelProtocolFields<'a> {
    pub protocol: &'a str,
    pub channel_type: Option<&'a str>,
    pub methods: Option<&'a [String]>,
    pub route_pattern: Option<&'a str>,
    pub topic: Option<&'a str>,
    pub consumer_group: Option<&'a str>,
}

/// Native Rust channel driver interface (`V2-11.1`).
///
/// Implemented by Community foundation drivers (`rest`, `http`, `cron`, `source`)
/// and Enterprise native channel drivers (`graphql`, `kafka`, `sse`).
pub trait ChannelDriver: Send + Sync {
    /// Lowercase wire protocol identifier (e.g. `"rest"`, `"http"`, `"cron"`, `"source"`).
    fn protocol_id(&self) -> &'static str;

    /// Transport and routing capabilities supported by this channel driver.
    fn capabilities(&self) -> ChannelDriverCapabilities;

    /// Validate protocol-conditional fields (`methods`, `route_pattern`, `topic`, etc.).
    fn validate_fields(&self, fields: &ChannelProtocolFields<'_>) -> Vec<FieldError>;

    /// Validate protocol-specific `transport_config` and `config` combinations.
    fn validate_transport(
        &self,
        _transport_config: &Value,
        _config: &Value,
        _channel_id: Option<&str>,
    ) -> Result<(), OrionError> {
        Ok(())
    }
}

/// Community `"rest"` channel driver.
pub struct RestChannelDriver;

impl ChannelDriver for RestChannelDriver {
    fn protocol_id(&self) -> &'static str {
        "rest"
    }

    fn capabilities(&self) -> ChannelDriverCapabilities {
        ChannelDriverCapabilities {
            supports_sync: true,
            supports_async: true,
            registers_http_routes: true,
            supports_oauth2_login: true,
        }
    }

    fn validate_fields(&self, fields: &ChannelProtocolFields<'_>) -> Vec<FieldError> {
        validate_http_like_fields(self.protocol_id(), fields)
    }
}

/// Community `"http"` channel driver.
pub struct HttpChannelDriver;

impl ChannelDriver for HttpChannelDriver {
    fn protocol_id(&self) -> &'static str {
        "http"
    }

    fn capabilities(&self) -> ChannelDriverCapabilities {
        ChannelDriverCapabilities {
            supports_sync: true,
            supports_async: true,
            registers_http_routes: true,
            supports_oauth2_login: false,
        }
    }

    fn validate_fields(&self, fields: &ChannelProtocolFields<'_>) -> Vec<FieldError> {
        validate_http_like_fields(self.protocol_id(), fields)
    }
}

fn validate_http_like_fields(
    protocol: &str,
    fields: &ChannelProtocolFields<'_>,
) -> Vec<FieldError> {
    let mut out = Vec::new();
    match fields.methods {
        Some(m) if !m.is_empty() => out.extend(crate::validation::channels::check_http_methods(m)),
        _ => out.push(
            FieldError::new(
                "channel.methods",
                "REQUIRED_FOR_PROTOCOL",
                format!(
                    "REST/HTTP channels must specify at least one HTTP method (protocol=\"{protocol}\")"
                ),
            )
            .with_expected(Value::String("non-empty array of method names".to_string())),
        ),
    }
    match fields.route_pattern {
        Some(r) if !r.trim().is_empty() => {
            out.extend(crate::validation::channels::check_route_pattern(r))
        }
        _ => out.push(
            FieldError::new(
                "channel.route_pattern",
                "REQUIRED_FOR_PROTOCOL",
                format!(
                    "REST/HTTP channels must specify a route_pattern (protocol=\"{protocol}\")"
                ),
            )
            .with_expected(Value::String(
                "URL path pattern (e.g. \"/orders/{id}\")".to_string(),
            )),
        ),
    }
    out
}

/// Community `"cron"` scheduler channel driver.
pub struct CronChannelDriver;

impl ChannelDriver for CronChannelDriver {
    fn protocol_id(&self) -> &'static str {
        "cron"
    }

    fn capabilities(&self) -> ChannelDriverCapabilities {
        ChannelDriverCapabilities {
            supports_sync: false,
            supports_async: true,
            registers_http_routes: false,
            supports_oauth2_login: false,
        }
    }

    fn validate_fields(&self, fields: &ChannelProtocolFields<'_>) -> Vec<FieldError> {
        crate::validation::cron::cron_routing_errors(
            fields.channel_type,
            fields.methods,
            fields.route_pattern,
            fields.topic,
            fields.consumer_group,
        )
    }

    fn validate_transport(
        &self,
        transport_config: &Value,
        config: &Value,
        channel_id: Option<&str>,
    ) -> Result<(), OrionError> {
        let mut details =
            crate::validation::cron::cron_transport_errors(transport_config, channel_id);
        details.extend(crate::validation::cron::cron_config_errors(config));
        if details.is_empty() {
            return Ok(());
        }
        Err(OrionError::Validation {
            code: "VALIDATION_ERROR",
            message: format!(
                "Cron channel has {} problem(s): a schedule that does not compile would \
                 be quarantined at load rather than serving",
                details.len()
            ),
            details,
        })
    }
}

/// Community `"source"` polling channel driver (`V2-15`, §7.2 of `v2-design.md`).
///
/// Drives any connector package (`rust`, `workflow`, or `wasm`) that implements
/// the `source` facet (`open` / `poll` / `settle` / `close`).
pub struct SourceChannelDriver;

impl ChannelDriver for SourceChannelDriver {
    fn protocol_id(&self) -> &'static str {
        "source"
    }

    fn capabilities(&self) -> ChannelDriverCapabilities {
        ChannelDriverCapabilities {
            supports_sync: false,
            supports_async: true,
            registers_http_routes: false,
            supports_oauth2_login: false,
        }
    }

    fn validate_fields(&self, fields: &ChannelProtocolFields<'_>) -> Vec<FieldError> {
        let mut out = Vec::new();
        if let Some(ct) = fields.channel_type
            && !ct.eq_ignore_ascii_case("async")
        {
            out.push(FieldError::new(
                "channel.channel_type",
                "INVALID",
                "Source channels must use channel_type 'async'",
            ));
        }
        if fields.route_pattern.is_some_and(|r| !r.trim().is_empty()) {
            out.push(FieldError::new(
                "channel.route_pattern",
                "INVALID",
                "Source channels do not register HTTP routes",
            ));
        }
        out
    }

    fn validate_transport(
        &self,
        transport_config: &Value,
        _config: &Value,
        _channel_id: Option<&str>,
    ) -> Result<(), OrionError> {
        let mut details = Vec::new();
        let source_obj = transport_config.get("source").and_then(Value::as_object);
        let Some(source) = source_obj else {
            details.push(FieldError::new(
                "channel.transport_config.source",
                "REQUIRED_FOR_PROTOCOL",
                "Source channels must declare transport_config.source",
            ));
            return Err(OrionError::Validation {
                code: "VALIDATION_ERROR",
                message: "Invalid source channel transport_config".to_string(),
                details,
            });
        };

        if source
            .get("connection_id")
            .and_then(Value::as_str)
            .is_none_or(|s| s.trim().is_empty())
        {
            details.push(FieldError::new(
                "channel.transport_config.source.connection_id",
                "REQUIRED_FOR_PROTOCOL",
                "Source channels must specify transport_config.source.connection_id",
            ));
        }
        if source
            .get("operation")
            .and_then(Value::as_str)
            .is_none_or(|s| s.trim().is_empty())
        {
            details.push(FieldError::new(
                "channel.transport_config.source.operation",
                "REQUIRED_FOR_PROTOCOL",
                "Source channels must specify transport_config.source.operation",
            ));
        }

        if details.is_empty() {
            Ok(())
        } else {
            Err(OrionError::Validation {
                code: "VALIDATION_ERROR",
                message: format!("Source channel has {} problem(s)", details.len()),
                details,
            })
        }
    }
}

/// Legacy v1 `"kafka"` channel driver adapter.
///
/// Kept in [`ChannelDriverRegistry::with_legacy_v1`] for v1 route compatibility
/// until the Kafka channel driver is extracted to `orion-enterprise` in Phase 3;
/// omitted from [`ChannelDriverRegistry::community`].
pub struct KafkaChannelDriver;

impl ChannelDriver for KafkaChannelDriver {
    fn protocol_id(&self) -> &'static str {
        "kafka"
    }

    fn capabilities(&self) -> ChannelDriverCapabilities {
        ChannelDriverCapabilities {
            supports_sync: false,
            supports_async: true,
            registers_http_routes: false,
            supports_oauth2_login: false,
        }
    }

    fn validate_fields(&self, fields: &ChannelProtocolFields<'_>) -> Vec<FieldError> {
        let mut out = Vec::new();
        if fields.topic.is_none_or(|t| t.trim().is_empty()) {
            out.push(FieldError::new(
                "channel.topic",
                "REQUIRED_FOR_PROTOCOL",
                "Kafka channels must specify a topic",
            ));
        }
        out
    }
}

/// Open registry of native Rust [`ChannelDriver`] implementations (`V2-11.1`).
#[derive(Clone, Default)]
pub struct ChannelDriverRegistry {
    drivers: Arc<RwLock<BTreeMap<String, Arc<dyn ChannelDriver>>>>,
}

impl std::fmt::Debug for ChannelDriverRegistry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let protocols = self.protocol_ids();
        f.debug_struct("ChannelDriverRegistry")
            .field("protocols", &protocols)
            .finish()
    }
}

impl ChannelDriverRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// Community Edition channel driver registry containing strictly the
    /// foundational drivers: `"rest"`, `"http"`, `"cron"`, and `"source"`.
    pub fn community() -> Self {
        let registry = Self::new();
        registry.register(Arc::new(RestChannelDriver));
        registry.register(Arc::new(HttpChannelDriver));
        registry.register(Arc::new(CronChannelDriver));
        registry.register(Arc::new(SourceChannelDriver));
        registry
    }

    /// Community foundation drivers plus the legacy v1 `"kafka"` driver used by
    /// existing `/api/v1/admin/channels` endpoints prior to Phase 3 extraction.
    pub fn with_legacy_v1() -> Self {
        let registry = Self::community();
        registry.register(Arc::new(KafkaChannelDriver));
        registry
    }

    /// Register or replace a native Rust [`ChannelDriver`].
    pub fn register(&self, driver: Arc<dyn ChannelDriver>) {
        if let Ok(mut guard) = self.drivers.write() {
            guard.insert(driver.protocol_id().to_ascii_lowercase(), driver);
        }
    }

    /// Look up a registered driver by case-insensitive protocol identifier.
    pub fn get(&self, protocol_id: &str) -> Option<Arc<dyn ChannelDriver>> {
        let key = protocol_id.to_ascii_lowercase();
        self.drivers.read().ok()?.get(&key).cloned()
    }

    /// List all registered protocol identifiers in sorted order.
    pub fn protocol_ids(&self) -> Vec<String> {
        self.drivers
            .read()
            .map(|g| g.keys().cloned().collect())
            .unwrap_or_default()
    }

    /// Whether `protocol_id` is registered and declares HTTP route registration.
    pub fn registers_http_routes(&self, protocol_id: &str) -> bool {
        self.get(protocol_id)
            .is_some_and(|d| d.capabilities().registers_http_routes)
    }

    /// Validate a channel against the registered [`ChannelDriver`] for `fields.protocol`.
    pub fn validate_channel(
        &self,
        fields: &ChannelProtocolFields<'_>,
        transport_config: &Value,
        config: &Value,
        channel_id: Option<&str>,
    ) -> Result<Vec<FieldError>, OrionError> {
        let Some(driver) = self.get(fields.protocol) else {
            return Err(OrionError::invalid_field(
                "channel.protocol",
                "UNSUPPORTED_PROTOCOL",
                format!(
                    "Channel protocol '{}' is not registered on this node (registered: {})",
                    fields.protocol,
                    self.protocol_ids().join(", ")
                ),
            ));
        };
        let field_errors = driver.validate_fields(fields);
        driver.validate_transport(transport_config, config, channel_id)?;
        Ok(field_errors)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    struct CustomGraphqlDriver;

    impl ChannelDriver for CustomGraphqlDriver {
        fn protocol_id(&self) -> &'static str {
            "graphql"
        }

        fn capabilities(&self) -> ChannelDriverCapabilities {
            ChannelDriverCapabilities {
                supports_sync: true,
                supports_async: false,
                registers_http_routes: true,
                supports_oauth2_login: false,
            }
        }

        fn validate_fields(&self, fields: &ChannelProtocolFields<'_>) -> Vec<FieldError> {
            let mut out = Vec::new();
            if fields.route_pattern.is_none_or(|r| r.trim().is_empty()) {
                out.push(FieldError::new(
                    "channel.route_pattern",
                    "REQUIRED_FOR_PROTOCOL",
                    "GraphQL channels must specify a route_pattern",
                ));
            }
            out
        }
    }

    #[test]
    fn community_registry_contains_foundation_drivers_only() {
        let reg = ChannelDriverRegistry::community();
        assert_eq!(reg.protocol_ids(), vec!["cron", "http", "rest", "source"]);
        assert!(reg.get("kafka").is_none());
        assert!(reg.registers_http_routes("rest"));
        assert!(reg.registers_http_routes("http"));
        assert!(!reg.registers_http_routes("cron"));
        assert!(!reg.registers_http_routes("source"));
    }

    #[test]
    fn external_enterprise_driver_registers_without_core_enum_changes() {
        let reg = ChannelDriverRegistry::community();
        reg.register(Arc::new(CustomGraphqlDriver));

        assert!(reg.get("graphql").is_some());
        assert!(reg.registers_http_routes("graphql"));

        let missing_route = ChannelProtocolFields {
            protocol: "graphql",
            channel_type: Some("sync"),
            methods: None,
            route_pattern: None,
            topic: None,
            consumer_group: None,
        };
        let errs = reg
            .validate_channel(&missing_route, &json!({}), &json!({}), None)
            .expect("driver runs");
        assert_eq!(errs.len(), 1);
        assert_eq!(errs[0].path, "channel.route_pattern");
    }

    #[test]
    fn source_channel_driver_validates_transport_config() {
        let reg = ChannelDriverRegistry::community();
        let fields = ChannelProtocolFields {
            protocol: "source",
            channel_type: Some("async"),
            methods: None,
            route_pattern: None,
            topic: None,
            consumer_group: None,
        };

        // Missing source block fails transport validation.
        assert!(
            reg.validate_channel(&fields, &json!({}), &json!({}), None)
                .is_err()
        );

        // Valid source block passes.
        let valid_transport = json!({
            "source": {
                "connection_id": "pg-source",
                "operation": "poll_outbox"
            }
        });
        let field_errs = reg
            .validate_channel(&fields, &valid_transport, &json!({}), None)
            .expect("valid source transport");
        assert!(field_errs.is_empty());
    }
}
