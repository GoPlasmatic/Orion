//! The connector provider trait: [`ProviderBackend`].
//!
//! A provider is registered under its [`BackendKey`]
//! ([`ProviderBackend::key`], [`ProviderBackend::kind`],
//! [`ProviderBackend::driver_id`]): a `rust` backend under its driver id
//! (`org.orion.http`), a `workflow` or `wasm` package under its digest. The
//! node dispatches each attempt of a call to it with an [`AttemptContext`].
//!
//! Providers that support `source` channel polling also implement
//! [`SourceConnectorFacet`] and return `Some(self)` from
//! [`ProviderBackend::as_source`].

use std::collections::BTreeMap;
use std::time::Instant;

use async_trait::async_trait;
use orion_api::Digest;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::attempt::AttemptContext;
use crate::contract::{ConnectorError, ConnectorOutput, ImplementationKind};
use crate::identity::{BackendKey, EffectiveConnectionIdentity, ProviderId, Revision};

/// Source batch item emitted by [`SourceConnectorFacet::poll`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct SourceBatchItem {
    pub delivery_id: String,
    pub payload: Value,
    #[serde(default)]
    pub metadata: BTreeMap<String, Value>,
}

impl SourceBatchItem {
    pub fn new(delivery_id: impl Into<String>, payload: Value) -> Self {
        Self {
            delivery_id: delivery_id.into(),
            payload,
            metadata: BTreeMap::new(),
        }
    }

    #[must_use]
    pub fn with_metadata(mut self, metadata: BTreeMap<String, Value>) -> Self {
        self.metadata = metadata;
        self
    }
}

/// Source poll batch returned by [`SourceConnectorFacet::poll`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct SourcePollBatch {
    pub items: Vec<SourceBatchItem>,
    pub next_checkpoint: Option<Value>,
}

impl SourcePollBatch {
    pub fn new(items: Vec<SourceBatchItem>, next_checkpoint: Option<Value>) -> Self {
        Self {
            items,
            next_checkpoint,
        }
    }
}

/// Settlement disposition reported by the native Rust `source` channel runner
/// back to [`SourceConnectorFacet::settle`] after workflow execution completes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum SourceSettlement {
    Ack,
    NackRetryable,
    DeadLetter,
}

/// Optional source facet (`open` / `poll` / `settle` / `close`) implemented by
/// source-capable connectors of any backend kind (`rust`, `workflow`, `wasm`)
/// and driven by the native Rust `source` channel runner.
#[async_trait]
pub trait SourceConnectorFacet: Send + Sync {
    /// Open or resume a source cursor from `checkpoint`.
    async fn open(
        &self,
        ctx: &AttemptContext,
        checkpoint: Option<Value>,
    ) -> Result<Value, ConnectorError>;

    /// Poll up to `max_items` records starting from `cursor`.
    async fn poll(
        &self,
        ctx: &AttemptContext,
        cursor: &Value,
        max_items: u32,
    ) -> Result<SourcePollBatch, ConnectorError>;

    /// Settle a delivered item (`Ack`, `NackRetryable`, or `DeadLetter`).
    async fn settle(
        &self,
        ctx: &AttemptContext,
        delivery_id: &str,
        settlement: SourceSettlement,
    ) -> Result<(), ConnectorError>;

    /// Close the source cursor and release any cursor-specific state.
    async fn close(&self, ctx: &AttemptContext, cursor: Value) -> Result<(), ConnectorError>;
}

/// One operation a provider declares ([`ProviderDescription`]): the names a
/// `connector_call` may give it, and the `orion-json-schema/1` documents its
/// input and its answer must satisfy, when it declares them. The node checks
/// the input before dispatch and the answer after, the way it checks an
/// uploaded package's `operations.<name>` schemas.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct OperationDescription {
    /// Other names `connector_call` may call it by (`cache.get` for `get`).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub aliases: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input_schema: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output_schema: Option<Value>,
}

impl OperationDescription {
    pub fn new() -> Self {
        Self::default()
    }

    /// Another name for the operation.
    #[must_use]
    pub fn with_alias(mut self, alias: impl Into<String>) -> Self {
        self.aliases.push(alias.into());
        self
    }

    /// The schema the call's input must satisfy.
    #[must_use]
    pub fn with_input_schema(mut self, schema: Value) -> Self {
        self.input_schema = Some(schema);
        self
    }

    /// The schema the provider's answer must satisfy.
    #[must_use]
    pub fn with_output_schema(mut self, schema: Value) -> Self {
        self.output_schema = Some(schema);
        self
    }
}

/// What a provider declares about itself ([`ProviderBackend::describe`]):
/// its operations, by canonical name. A provider that declares one declares
/// all of them: a `connector_call` naming any other is refused when its
/// workflow loads.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct ProviderDescription {
    pub operations: BTreeMap<String, OperationDescription>,
}

impl ProviderDescription {
    pub fn new() -> Self {
        Self::default()
    }

    /// Declare `name`.
    #[must_use]
    pub fn with_operation(
        mut self,
        name: impl Into<String>,
        operation: OperationDescription,
    ) -> Self {
        self.operations.insert(name.into(), operation);
        self
    }

    /// The operation `name` calls, by its canonical name or an alias,
    /// compared case-insensitively.
    pub fn operation(&self, name: &str) -> Option<(&str, &OperationDescription)> {
        self.operations
            .iter()
            .find(|(canonical, op)| {
                canonical.eq_ignore_ascii_case(name)
                    || op.aliases.iter().any(|a| a.eq_ignore_ascii_case(name))
            })
            .map(|(canonical, op)| (canonical.as_str(), op))
    }
}

/// What a provider is asked to validate: a connection's configuration and,
/// when it belongs to a stored connection, which one.
#[derive(Debug, Clone, Copy)]
#[non_exhaustive]
pub struct ValidateContext<'a> {
    config: &'a Value,
    connection_id: Option<&'a str>,
    revision: Option<Revision>,
}

impl<'a> ValidateContext<'a> {
    /// A bare configuration, not yet tied to a connection.
    pub fn new(config: &'a Value) -> Self {
        Self {
            config,
            connection_id: None,
            revision: None,
        }
    }

    /// Tie the configuration to a connection revision.
    #[must_use]
    pub fn for_connection(mut self, connection_id: &'a str, revision: Revision) -> Self {
        self.connection_id = Some(connection_id);
        self.revision = Some(revision);
        self
    }

    pub fn config(&self) -> &'a Value {
        self.config
    }

    pub fn connection_id(&self) -> Option<&'a str> {
        self.connection_id
    }

    pub fn revision(&self) -> Option<Revision> {
        self.revision
    }
}

/// Unified connector provider backend trait.
///
/// Providers register implementations of this trait through
/// [`Registrar::provider_backend`](crate::Registrar::provider_backend) without
/// modifying core enums or built-in handler tables. Implement it with
/// `#[async_trait::async_trait]`.
#[async_trait]
pub trait ProviderBackend: Send + Sync {
    /// Implementation kind (`Rust`, `Workflow`, or `Wasm`).
    fn kind(&self) -> ImplementationKind;

    /// The id the backend registers under: a `rust` backend's driver id (its
    /// [`ProviderId`](crate::ProviderId), e.g. `"org.orion.http"`),
    /// or a `workflow`/`wasm` backend's package digest.
    fn driver_id(&self) -> &str;

    /// Typed registration key (`BackendKey::Rust(ProviderId)`,
    /// `BackendKey::Workflow(Digest)`, or `BackendKey::Wasm(Digest)`).
    fn key(&self) -> BackendKey {
        match self.kind() {
            ImplementationKind::Rust => BackendKey::Rust(ProviderId::from(self.driver_id())),
            ImplementationKind::Workflow => BackendKey::Workflow(
                Digest::parse(self.driver_id())
                    .expect("workflow ProviderBackend::driver_id must be a valid sha256 digest"),
            ),
            ImplementationKind::Wasm => BackendKey::Wasm(
                Digest::parse(self.driver_id())
                    .expect("wasm ProviderBackend::driver_id must be a valid sha256 digest"),
            ),
        }
    }

    /// Validate resolved connection configuration against provider-specific invariants.
    fn validate_config(&self, config: &Value) -> Result<(), ConnectorError>;

    /// Validate a connection's configuration with the connection around it.
    /// The host calls this one; the default defers to
    /// [`validate_config`](Self::validate_config), so a provider that needs the
    /// connection's identity or revision overrides it instead.
    fn validate_connection(&self, ctx: &ValidateContext<'_>) -> Result<(), ConnectorError> {
        self.validate_config(ctx.config())
    }

    /// Execute one attempt of `operation` with validated `input`.
    async fn invoke(
        &self,
        ctx: &AttemptContext,
        operation: &str,
        input: Value,
    ) -> Result<ConnectorOutput, ConnectorError>;

    /// Probe downstream connectivity or readiness under `ctx`.
    async fn health_check(&self, ctx: &AttemptContext) -> Result<(), ConnectorError> {
        let _ = ctx;
        Ok(())
    }

    /// Release what the provider holds for `identity` (a pooled client, a
    /// producer). The node calls it once the generation it publishes no longer
    /// pins that connection revision: the connection was removed, or replaced
    /// by a new revision. A call already admitted on the old generation may
    /// still hold what it acquired, so release idle resources rather than
    /// tearing down ones in use.
    async fn dispose(&self, identity: &EffectiveConnectionIdentity) {
        let _ = identity;
    }

    /// Flush and close, by `deadline`: the node is shutting down. Called once,
    /// after the channel drivers have stopped and before the trace queues
    /// drain, so a provider that buffers writes (a Kafka producer) can deliver
    /// them. Nothing is dispatched to the provider after it.
    async fn shutdown(&self, deadline: Instant) {
        let _ = deadline;
    }

    /// Input keys of `operation` that must be authored as literal strings:
    /// `connector_call` refuses a task whose `input` builds one from the
    /// message, at load. A SQL provider names its statement keys, so a
    /// caller's values reach the database only as bind parameters.
    fn literal_input_keys(&self, _operation: &str) -> Vec<String> {
        Vec::new()
    }

    /// The operations this provider serves, with their input and output
    /// schemas. `None` (the default) declares nothing: any operation name
    /// reaches [`Self::invoke`], which judges it. A description is enforced:
    /// an undeclared operation is refused when its workflow loads, and a
    /// declared schema is checked on every call. `/runtime-capabilities`
    /// serves it.
    fn describe(&self) -> Option<ProviderDescription> {
        None
    }

    /// Return the optional [`SourceConnectorFacet`] if this provider supports
    /// `source` channel polling.
    fn as_source(&self) -> Option<&dyn SourceConnectorFacet> {
        None
    }
}
