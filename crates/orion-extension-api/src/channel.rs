//! The channel driver trait: what decides whether a channel of a protocol
//! is valid on a node, whether it claims an HTTP route, and, for a protocol
//! whose messages arrive some other way, how its listener starts.
//!
//! A channel's `protocol` names a driver. A node refuses a channel whose
//! protocol no registered driver serves, at upload and at load.

use std::sync::Arc;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::FieldError;
use crate::ingress::{ChannelBinding, DriverError, GuardSet, IngressHost, RunningDriver};

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
    /// The driver runs its own listener ([`ChannelDriver::start`]) and its
    /// deliveries pass these guards. `None` for a protocol served by the
    /// node's own ingresses (the HTTP data plane, the cron scheduler).
    pub ingress: Option<GuardSet>,
}

impl ChannelDriverCapabilities {
    /// A driver that declares no capability.
    pub const fn none() -> Self {
        Self {
            supports_sync: false,
            supports_async: false,
            registers_http_routes: false,
            supports_oauth2_login: false,
            ingress: None,
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

    /// Runs its own listener, whose deliveries pass `guards`. Authoring then
    /// refuses a channel of this protocol that configures any other guard.
    #[must_use]
    pub const fn ingress(mut self, guards: GuardSet) -> Self {
        self.ingress = Some(guards);
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
        }
    }
}

/// A native Rust channel driver: one channel protocol.
///
/// A node registers a set of drivers through its composition; each serves one
/// protocol.
#[async_trait]
pub trait ChannelDriver: Send + Sync {
    /// Lowercase wire protocol identifier (e.g. `"rest"`, `"http"`, `"cron"`).
    fn protocol_id(&self) -> &str;

    /// Transport and routing capabilities supported by this channel driver.
    fn capabilities(&self) -> ChannelDriverCapabilities;

    /// Validate protocol-conditional fields (`channel_type`, `methods`, `route_pattern`).
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

    /// Every channel of this protocol in a candidate deployment, together:
    /// for a rule no single channel can break alone (two Kafka channels
    /// consuming one topic in one group). Each problem refuses the candidate
    /// with `DEPLOYMENT_WOULD_NOT_BUILD`, at plan, at apply and on every node
    /// that builds it. Name the channel in each path
    /// (`channel.<id>.transport_config.topics`).
    fn validate_set(&self, channels: &[ChannelBinding]) -> Vec<FieldError> {
        let _ = channels;
        Vec::new()
    }

    /// An `orion-json-schema/1` document for this protocol's
    /// `transport_config`. The node validates every channel of the protocol
    /// against it at upload, and serves it in `/runtime-capabilities` so the
    /// offline tools (`lint`, `compile --capabilities`) validate an edition's
    /// channels without the edition's binary. `None` leaves the shape to
    /// [`Self::validate_transport`] alone.
    fn transport_schema(&self) -> Option<Value> {
        None
    }

    /// Start the driver's listener, once, at boot. `host` dispatches the
    /// protocol's deliveries (see [`crate::ingress`]). A driver that declares
    /// [`ChannelDriverCapabilities::ingress`] returns its running half; the
    /// default serves none.
    async fn start(
        &self,
        host: Arc<dyn IngressHost>,
    ) -> Result<Option<Box<dyn RunningDriver>>, DriverError> {
        let _ = host;
        Ok(None)
    }
}
