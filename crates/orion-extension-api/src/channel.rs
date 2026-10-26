//! The channel driver trait: what decides whether a channel of a protocol
//! is valid on a node, and whether it claims an HTTP route.
//!
//! A channel's `protocol` names a driver. A node refuses a channel whose
//! protocol no registered driver serves, at upload and at load.

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::FieldError;

/// Capabilities declared by a [`ChannelDriver`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChannelDriverCapabilities {
    pub supports_sync: bool,
    pub supports_async: bool,
    pub registers_http_routes: bool,
    pub supports_oauth2_login: bool,
}

/// The protocol-conditional fields of a channel, as validation hands them
/// to its driver.
#[derive(Debug, Clone)]
pub struct ChannelProtocolFields<'a> {
    pub protocol: &'a str,
    pub channel_type: Option<&'a str>,
    pub methods: Option<&'a [String]>,
    pub route_pattern: Option<&'a str>,
    pub topic: Option<&'a str>,
    pub consumer_group: Option<&'a str>,
}

/// A native Rust channel driver: one channel protocol.
///
/// Community registers `rest`, `http` and `cron`; an edition registers more
/// through its composition.
pub trait ChannelDriver: Send + Sync {
    /// Lowercase wire protocol identifier (e.g. `"rest"`, `"http"`, `"cron"`).
    fn protocol_id(&self) -> &'static str;

    /// Transport and routing capabilities supported by this channel driver.
    fn capabilities(&self) -> ChannelDriverCapabilities;

    /// Validate protocol-conditional fields (`methods`, `route_pattern`, `topic`, etc.).
    fn validate_fields(&self, fields: &ChannelProtocolFields<'_>) -> Vec<FieldError>;

    /// Validate protocol-specific `transport_config` and `config` combinations,
    /// one [`FieldError`] per problem (paths rooted at `channel.`).
    fn validate_transport(
        &self,
        _transport_config: &Value,
        _config: &Value,
        _channel_id: Option<&str>,
    ) -> Vec<FieldError> {
        Vec::new()
    }
}
