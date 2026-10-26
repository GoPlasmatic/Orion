//! The connector provider traits: [`ProviderBackend`], and the optional
//! [`SourceConnectorFacet`] a source-capable provider adds.
//!
//! A provider is registered under its kind and backend id
//! ([`ProviderBackend::kind`], [`ProviderBackend::driver_id`]): a `rust`
//! backend under its driver id (`org.orion.http`), a `workflow` or `wasm`
//! package under its digest. The node dispatches each attempt of a call to
//! it with an [`AttemptContext`].

use std::collections::BTreeMap;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::attempt::AttemptContext;
use crate::contract::{ConnectorError, ConnectorOutput, ImplementationKind};
use crate::identity::EffectiveConnectionIdentity;

/// Source batch item emitted by [`SourceConnectorFacet::poll`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SourceBatchItem {
    pub delivery_id: String,
    pub payload: Value,
    #[serde(default)]
    pub metadata: BTreeMap<String, Value>,
}

/// Source poll batch returned by [`SourceConnectorFacet::poll`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SourcePollBatch {
    pub items: Vec<SourceBatchItem>,
    pub next_checkpoint: Option<Value>,
}

/// Settlement disposition reported by the native Rust `source` channel runner
/// back to [`SourceConnectorFacet::settle`] after workflow execution completes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
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

/// Unified connector provider backend trait.
///
/// External Community and Enterprise providers register implementations of this
/// trait without modifying core enums or built-in handler tables. Implement it
/// with `#[async_trait::async_trait]`.
#[async_trait]
pub trait ProviderBackend: Send + Sync {
    /// Implementation kind (`Rust`, `Workflow`, or `Wasm`).
    fn kind(&self) -> ImplementationKind;

    /// The id the backend registers under: a `rust` backend's driver id (its
    /// [`ProviderId`](crate::ProviderId), e.g. `"org.orion.http"`),
    /// or a `workflow`/`wasm` backend's package digest.
    fn driver_id(&self) -> &str;

    /// Validate resolved connection configuration against provider-specific invariants.
    fn validate_config(&self, config: &Value) -> Result<(), ConnectorError>;

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

    /// Dispose any provider-held resources for `identity`.
    async fn dispose(&self, identity: &EffectiveConnectionIdentity) {
        let _ = identity;
    }

    /// Input keys of `operation` that must be authored as literal strings:
    /// `connector_call` refuses a task whose `input` builds one from the
    /// message, at load. A SQL provider names its statement keys, so a
    /// caller's values reach the database only as bind parameters.
    fn literal_input_keys(&self, _operation: &str) -> &'static [&'static str] {
        &[]
    }

    /// Return the optional [`SourceConnectorFacet`] if this provider supports
    /// `source` channel polling.
    fn as_source(&self) -> Option<&dyn SourceConnectorFacet> {
        None
    }
}
