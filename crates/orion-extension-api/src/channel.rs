//! The channel driver trait: what decides whether a channel of a protocol
//! is valid on a node, and whether it claims an HTTP route.
//!
//! A channel's `protocol` names a driver. A node refuses a channel whose
//! protocol no registered driver serves, at upload and at load.

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::FieldError;

/// Capabilities declared by a [`ChannelDriver`].
///
/// Start from [`ChannelDriverCapabilities::none`] and enable what the driver
/// does; the struct grows as capabilities are added, so it cannot be built
/// field by field outside this crate.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct ChannelDriverCapabilities {
    pub supports_sync: bool,
    pub supports_async: bool,
    pub registers_http_routes: bool,
    pub supports_oauth2_login: bool,
}

impl ChannelDriverCapabilities {
    /// A driver that declares no capability.
    pub const fn none() -> Self {
        Self {
            supports_sync: false,
            supports_async: false,
            registers_http_routes: false,
            supports_oauth2_login: false,
        }
    }

    /// Serves synchronous calls.
    #[must_use]
    pub const fn sync(mut self) -> Self {
        self.supports_sync = true;
        self
    }

    /// Accepts asynchronous submissions.
    #[must_use]
    pub const fn async_submit(mut self) -> Self {
        self.supports_async = true;
        self
    }

    /// Claims HTTP routes on the data plane.
    #[must_use]
    pub const fn http_routes(mut self) -> Self {
        self.registers_http_routes = true;
        self
    }

    /// Supports OAuth2 login.
    #[must_use]
    pub const fn oauth2_login(mut self) -> Self {
        self.supports_oauth2_login = true;
        self
    }
}

/// The protocol-conditional fields of a channel, as validation hands them
/// to its driver.
///
/// Core builds it; a driver reads the fields. Not constructible field by field
/// outside this crate, so a new field is not a breaking change.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct ChannelProtocolFields<'a> {
    pub protocol: &'a str,
    pub channel_type: Option<&'a str>,
    pub methods: Option<&'a [String]>,
    pub route_pattern: Option<&'a str>,
    pub topic: Option<&'a str>,
    pub consumer_group: Option<&'a str>,
}

impl<'a> ChannelProtocolFields<'a> {
    /// The fields of a channel of `protocol`, all others unset; assign the ones
    /// the channel carries.
    pub fn new(protocol: &'a str) -> Self {
        Self {
            protocol,
            channel_type: None,
            methods: None,
            route_pattern: None,
            topic: None,
            consumer_group: None,
        }
    }
}

/// A native Rust channel driver: one channel protocol.
///
/// A node registers a set of drivers through its composition; each serves one
/// protocol.
pub trait ChannelDriver: Send + Sync {
    /// Lowercase wire protocol identifier (e.g. `"rest"`, `"http"`, `"cron"`).
    fn protocol_id(&self) -> &str;

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
