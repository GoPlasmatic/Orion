//! Channel ingress: how a driver whose messages do not arrive over the HTTP
//! data plane hands them to the node.
//!
//! The split is fixed. The **host** owns the run: it loads the runtime
//! generation once per delivery, resolves the channel, applies the guards the
//! driver declared ([`GuardSet`]), writes the trace, executes the channel's
//! workflows and records the common metrics, all in
//! [`IngressHost::dispatch`]. The **driver** owns the transport: connecting,
//! reading, decoding, retries, acknowledgement or offsets, its dead-letter
//! destination and its own metrics.
//!
//! The lifecycle, all driven by the host:
//!
//! 1. At boot, [`ChannelDriver::start`](crate::ChannelDriver::start) is called
//!    once with the [`IngressHost`] for the driver's protocol. A driver that
//!    serves ingress returns its [`RunningDriver`].
//! 2. After every runtime generation the node publishes,
//!    [`RunningDriver::reconcile`] receives the serviceable channels of the
//!    protocol. Calls are serial per driver and always carry the latest
//!    generation, so a burst of publishes may skip one but never goes back.
//! 3. At shutdown, [`RunningDriver::shutdown`] is called before the trace
//!    queues drain, so deliveries still in flight are traced.

use std::time::Instant;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

/// The core guards a protocol's deliveries pass through.
///
/// Start from [`GuardSet::none`] and enable what applies. Authentication,
/// the origin allow-list, the response cache and the OAuth2 sign-in flow are
/// HTTP-only and cannot be enabled here; a channel of an ingress protocol that
/// configures them, or a guard its protocol leaves off, is refused at upload.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct GuardSet {
    /// The channel's `rate_limit` (token bucket or shared window).
    pub rate_limit: bool,
    /// The channel's `validation_logic`.
    pub validation: bool,
    /// Idempotency-key deduplication (`deduplication`).
    pub deduplication: bool,
    /// The per-channel concurrency permit (`backpressure`).
    pub backpressure: bool,
}

impl GuardSet {
    /// No guard.
    pub const fn none() -> Self {
        Self {
            rate_limit: false,
            validation: false,
            deduplication: false,
            backpressure: false,
        }
    }

    /// Apply the channel's rate limit.
    #[must_use]
    pub const fn rate_limit(mut self) -> Self {
        self.rate_limit = true;
        self
    }

    /// Apply the channel's `validation_logic`.
    #[must_use]
    pub const fn validation(mut self) -> Self {
        self.validation = true;
        self
    }

    /// Apply idempotency-key deduplication.
    #[must_use]
    pub const fn deduplication(mut self) -> Self {
        self.deduplication = true;
        self
    }

    /// Hold the channel's concurrency permit for the run.
    #[must_use]
    pub const fn backpressure(mut self) -> Self {
        self.backpressure = true;
        self
    }
}

/// One serviceable channel of a driver's protocol, as
/// [`RunningDriver::reconcile`] hands it over. Built by the host.
#[derive(Debug, Clone, PartialEq)]
#[non_exhaustive]
pub struct ChannelBinding {
    channel_id: String,
    name: String,
    fingerprint: String,
    transport_config: Value,
    timeout_ms: Option<u64>,
}

impl ChannelBinding {
    /// A binding for the channel `channel_id`, named `name`, whose stored
    /// definition hashes to `fingerprint`.
    pub fn new(
        channel_id: impl Into<String>,
        name: impl Into<String>,
        fingerprint: impl Into<String>,
        transport_config: Value,
    ) -> Self {
        Self {
            channel_id: channel_id.into(),
            name: name.into(),
            fingerprint: fingerprint.into(),
            transport_config,
            timeout_ms: None,
        }
    }

    /// The channel's own `timeout_ms`.
    #[must_use]
    pub fn with_timeout_ms(mut self, timeout_ms: Option<u64>) -> Self {
        self.timeout_ms = timeout_ms;
        self
    }

    /// The channel's stable id: what [`Delivery::new`] names.
    pub fn channel_id(&self) -> &str {
        &self.channel_id
    }

    /// The channel's name, for logs and metric labels.
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Changes whenever anything in the channel's definition changes. A
    /// driver compares it to decide whether a listener must be rebuilt.
    pub fn fingerprint(&self) -> &str {
        &self.fingerprint
    }

    /// The channel's `transport_config`, already accepted by
    /// [`ChannelDriver::validate_transport`](crate::ChannelDriver::validate_transport).
    pub fn transport_config(&self) -> &Value {
        &self.transport_config
    }

    /// The channel's own `timeout_ms`, when it sets one.
    pub fn timeout_ms(&self) -> Option<u64> {
        self.timeout_ms
    }
}

/// One message for [`IngressHost::dispatch`]. Start from [`Delivery::new`].
#[derive(Debug, Clone, PartialEq)]
#[non_exhaustive]
pub struct Delivery {
    /// The target channel's stable id ([`ChannelBinding::channel_id`]).
    pub channel_id: String,
    /// The workflow input, `data`.
    pub data: Value,
    /// Transport facts, stamped at `metadata.trigger` with `type` set to the
    /// protocol id (a `type` given here is replaced).
    pub trigger: Map<String, Value>,
    /// The default rate-limit bucket. The channel id when unset.
    pub caller_identity: Option<String>,
    /// Header-like attributes for `rate_limit.key_logic` and the
    /// deduplication header, matched case-insensitively.
    pub headers: Vec<(String, String)>,
    /// The idempotency key to use when the deduplication header is absent.
    pub dedup_key: Option<String>,
    /// The identity of this physical delivery: a redelivery presenting the
    /// same owner is recognised as itself rather than refused as a duplicate.
    pub dedup_owner: Option<String>,
    /// The run's deadline when the channel sets no `timeout_ms`, and a
    /// ceiling on it when it does: the channel may shorten it but not
    /// lengthen it. With neither, the run has no deadline of its own.
    pub max_timeout_ms: Option<u64>,
}

impl Delivery {
    /// A delivery of `data` to the channel `channel_id`.
    pub fn new(channel_id: impl Into<String>, data: Value) -> Self {
        Self {
            channel_id: channel_id.into(),
            data,
            trigger: Map::new(),
            caller_identity: None,
            headers: Vec::new(),
            dedup_key: None,
            dedup_owner: None,
            max_timeout_ms: None,
        }
    }

    /// The transport facts for `metadata.trigger`.
    #[must_use]
    pub fn with_trigger(mut self, trigger: Map<String, Value>) -> Self {
        self.trigger = trigger;
        self
    }

    /// The default rate-limit bucket.
    #[must_use]
    pub fn with_caller_identity(mut self, identity: impl Into<String>) -> Self {
        self.caller_identity = Some(identity.into());
        self
    }

    /// One header-like attribute.
    #[must_use]
    pub fn with_header(mut self, name: impl Into<String>, value: impl Into<String>) -> Self {
        self.headers.push((name.into(), value.into()));
        self
    }

    /// The fallback idempotency key.
    #[must_use]
    pub fn with_dedup_key(mut self, key: impl Into<String>) -> Self {
        self.dedup_key = Some(key.into());
        self
    }

    /// The identity of this physical delivery.
    #[must_use]
    pub fn with_dedup_owner(mut self, owner: impl Into<String>) -> Self {
        self.dedup_owner = Some(owner.into());
        self
    }

    /// The deadline, and the ceiling on the channel's `timeout_ms`.
    #[must_use]
    pub fn with_max_timeout_ms(mut self, max_timeout_ms: u64) -> Self {
        self.max_timeout_ms = Some(max_timeout_ms);
        self
    }
}

/// Why a dispatched run did not complete.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum FailureKind {
    /// The run's deadline expired.
    Timeout,
    /// The engine failed the call.
    Engine,
    /// The run completed and a task reported an error.
    Workflow,
}

/// What became of one delivery. A driver decides from it whether to
/// acknowledge, retry or dead-letter, then reports that through the
/// [`Settle`] handle it came with.
#[derive(Debug, Clone, PartialEq)]
#[non_exhaustive]
pub enum DispatchOutcome {
    /// The workflows ran without error. `output` is the message's `data`.
    Completed { output: Value },
    /// The deduplication guard has already seen this key settled.
    /// Acknowledge it; nothing ran.
    Duplicate,
    /// Refused for a cause that clears on its own (a rate limit, the
    /// concurrency permit). Retry later; do not acknowledge.
    Deferred { reason: String },
    /// The channel refused the message (`validation_logic`, an unevaluable
    /// rate-limit key, …). Retrying it will fail the same way.
    Rejected { code: String, message: String },
    /// The run started and did not complete.
    Failed { kind: FailureKind, message: String },
    /// The channel is not serviceable in the current generation: absent,
    /// quarantined, or of another protocol. Nothing ran.
    Unavailable { reason: String },
}

/// How a driver reports what it did with a delivery: the half of the
/// deduplication contract the host cannot see.
///
/// Call exactly one. Dropping the handle without either leaves the claim
/// holding this delivery's owner token, so a redelivery presenting the same
/// [`Delivery::dedup_owner`] is still processed: at least once, never lost.
#[async_trait]
pub trait Settle: Send + Sync {
    /// The delivery is durably accounted for (acknowledged, or preserved in a
    /// dead-letter destination). Its key is settled for the window.
    async fn committed(self: Box<Self>);

    /// The delivery will be redelivered. Its key is released.
    async fn abandoned(self: Box<Self>);
}

/// A [`Settle`] with nothing to settle.
#[derive(Debug, Default, Clone, Copy)]
pub struct NothingToSettle;

#[async_trait]
impl Settle for NothingToSettle {
    async fn committed(self: Box<Self>) {}
    async fn abandoned(self: Box<Self>) {}
}

/// [`IngressHost::dispatch`]'s answer.
#[non_exhaustive]
pub struct Dispatched {
    pub outcome: DispatchOutcome,
    pub settle: Box<dyn Settle>,
}

impl Dispatched {
    pub fn new(outcome: DispatchOutcome, settle: Box<dyn Settle>) -> Self {
        Self { outcome, settle }
    }

    /// An outcome with nothing to settle.
    pub fn unsettled(outcome: DispatchOutcome) -> Self {
        Self::new(outcome, Box::new(NothingToSettle))
    }
}

impl std::fmt::Debug for Dispatched {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Dispatched")
            .field("outcome", &self.outcome)
            .finish_non_exhaustive()
    }
}

/// The node a driver runs on.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct NodeInfo {
    instance_id: String,
    cluster: bool,
}

impl NodeInfo {
    pub fn new(instance_id: impl Into<String>, cluster: bool) -> Self {
        Self {
            instance_id: instance_id.into(),
            cluster,
        }
    }

    /// The node's stable instance id (`cluster.instance_id`).
    pub fn instance_id(&self) -> &str {
        &self.instance_id
    }

    /// Whether the node runs in cluster mode.
    pub fn is_cluster(&self) -> bool {
        self.cluster
    }
}

/// A running driver's health, shown as its protocol's `/health` component.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "state", content = "reason")]
#[non_exhaustive]
pub enum DriverHealth {
    /// Serving.
    Ok,
    /// Not serving as configured. Also fails `/readyz`.
    Degraded(String),
    /// Nothing to serve (no channel of the protocol is deployed).
    Disabled,
}

impl DriverHealth {
    /// The `/health` component value: `ok`, `degraded` or `disabled`.
    pub fn component(&self) -> &'static str {
        match self {
            Self::Ok => "ok",
            Self::Degraded(_) => "degraded",
            Self::Disabled => "disabled",
        }
    }
}

/// A driver could not start.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{message}")]
pub struct DriverError {
    message: String,
}

impl DriverError {
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }

    pub fn message(&self) -> &str {
        &self.message
    }
}

/// What the node offers a running driver. Implemented by the host, one per
/// protocol.
#[async_trait]
pub trait IngressHost: Send + Sync {
    /// The protocol this host dispatches for.
    fn protocol(&self) -> &str;

    /// The node this driver runs on.
    fn node(&self) -> &NodeInfo;

    /// Run one delivery through its channel: load the generation once, apply
    /// the protocol's guards, write the trace (`mode` = the protocol id, input
    /// kept), execute, record `orion_messages_total` and
    /// `orion_message_duration_seconds`.
    async fn dispatch(&self, delivery: Delivery) -> Dispatched;
}

/// The running half of an ingress driver, returned by
/// [`ChannelDriver::start`](crate::ChannelDriver::start).
#[async_trait]
pub trait RunningDriver: Send + Sync {
    /// Every serviceable channel of the protocol in the generation the node
    /// just published. Quarantined channels are absent; an empty set means
    /// stop consuming.
    async fn reconcile(&self, channels: Vec<ChannelBinding>);

    /// The driver's current health.
    fn health(&self) -> DriverHealth;

    /// Finish in-flight deliveries and stop, by `deadline`. Called once.
    async fn shutdown(&self, deadline: Instant);
}
