//! Native Rust `source` channel runner, fenced partition leases and contiguous checkpoint ledger.
//!
//! Implements the host-owned `source` channel execution lifecycle:
//! - **Bounded Privileged Source Dispatch**: Only [`SourceChannelRunner`]
//!   can mint `orion.internal.source.*` invocations (`open`, `poll`, `settle`,
//!   `close`), enforcing batch-size and cursor-byte ceilings across all three
//!   connector backend kinds (`rust`, `workflow`, `wasm`).
//! - **Fenced Partition Ownership**: [`SourceLeaseCoordinator`]
//!   assigns monotonically increasing fence tokens per `(channel_id, partition_id)`
//!   in the `source_claims` table, with leases judged on the database clock on
//!   every backend. Stale owners after lease expiry or revision takeover cannot
//!   renew, acknowledge, or commit checkpoints.
//! - **Settlement Intent & Contiguous Checkpoints**:
//!   [`ContiguousCheckpointLedger`] records durable settlement intent before
//!   external ack/DLQ persistence, blocks checkpoint advancement across gaps or
//!   unpersisted DLQ items, and commits only contiguous settled offsets under
//!   the active fence token, to `source_checkpoints`.
//!
//! No channel protocol drives this yet: Community has no source provider (the
//! Kafka one moved to Orion Enterprise), so a `source` protocol would accept
//! channels nothing runs. An edition with a source provider registers the
//! channel driver.
//! - **Revision Handoff**: Transfers partition ownership to a new
//!   deployment revision with a higher fence token while enforcing checkpoint
//!   namespace compatibility.

mod lease;
mod ledger;

pub use lease::{
    SourceCoordinationError, SourceLeaseCoordinator, SourcePartitionClaim, SourcePartitionKey,
};
pub use ledger::{ContiguousCheckpointLedger, ItemSettlementState};

use std::sync::Arc;

use serde_json::Value;

use super::context::RunContext;
use super::provider_backend::ProviderRegistry;
use super::{
    ConnectionReference, ConnectorError, EffectStatus, EffectiveConnectionIdentity,
    ImplementationKind, codes,
};
use super::{SourcePollBatch, SourceSettlement};

/// Bounds enforced by [`SourceChannelRunner`] on every source facet call.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SourceRunnerLimits {
    pub max_batch_items: u32,
    pub max_cursor_bytes: usize,
}

impl Default for SourceRunnerLimits {
    fn default() -> Self {
        Self {
            max_batch_items: 100,
            max_cursor_bytes: 64 * 1024,
        }
    }
}

/// Host-owned native Rust `source` channel runner.
///
/// Drives any registered connector (`rust`, `workflow`, or `wasm`) that exposes
/// `super::SourceConnectorFacet`.
///
/// Revocation and the security lease are checked on the registry's
/// [`super::RevocationLedger`] before every facet call.
pub struct SourceChannelRunner {
    providers: ProviderRegistry,
    limits: SourceRunnerLimits,
}

impl SourceChannelRunner {
    pub fn new(providers: ProviderRegistry, limits: SourceRunnerLimits) -> Self {
        Self { providers, limits }
    }

    fn check_cursor_size(&self, cursor: &Value) -> Result<(), ConnectorError> {
        let encoded = serde_json::to_string(cursor).map_err(|e| {
            ConnectorError::contract_violation(
                "source_cursor",
                format!("Failed to serialize source cursor: {e}"),
                EffectStatus::Completed,
            )
        })?;
        if encoded.len() > self.limits.max_cursor_bytes {
            return Err(ConnectorError::contract_violation(
                "source_cursor",
                format!(
                    "Source cursor size ({} bytes) exceeds limit ({} bytes)",
                    encoded.len(),
                    self.limits.max_cursor_bytes
                ),
                EffectStatus::Completed,
            ));
        }
        Ok(())
    }

    /// The privileged half every source facet call shares: the runtime lease
    /// is still held, a source invocation for `step` is started for the
    /// target's connection, and its provider is registered. What comes back
    /// holds the invocation, its attempt and the backend.
    fn authorize(
        &self,
        run_ctx: &RunContext,
        target: SourceTarget,
        step: &str,
    ) -> Result<SourceCall, ConnectorError> {
        let SourceTarget {
            connection,
            effective_identity,
            kind,
            driver_id,
        } = target;
        self.providers.ledger().check_runtime_lease(
            run_ctx.generation_id(),
            &connection.provider.provider_id,
            &connection.connection_id,
            EffectStatus::NotStarted,
        )?;
        let invocation =
            run_ctx.start_privileged_source_invocation(connection, effective_identity, step, 1)?;
        let attempt = invocation.next_attempt()?;
        let backend = self.providers.get(kind, &driver_id).ok_or_else(|| {
            ConnectorError::forbidden(
                codes::CONNECTION_FORBIDDEN,
                "dispatch",
                format!("Source provider ({kind}, '{driver_id}') is not registered"),
                EffectStatus::NotStarted,
            )
        })?;
        Ok(SourceCall {
            _invocation: invocation,
            attempt,
            backend,
            driver_id,
        })
    }

    /// Authorize and execute `source.open` on a source-capable connector.
    pub async fn open_cursor(
        &self,
        run_ctx: &RunContext,
        target: SourceTarget,
        checkpoint: Option<Value>,
    ) -> Result<Value, ConnectorError> {
        let call = self.authorize(run_ctx, target, "open")?;
        let cursor = call.source()?.open(&call.attempt, checkpoint).await?;
        self.check_cursor_size(&cursor)?;
        Ok(cursor)
    }

    /// Authorize and execute `source.poll`, enforcing `max_batch_items` and cursor bounds.
    pub async fn poll_batch(
        &self,
        run_ctx: &RunContext,
        target: SourceTarget,
        cursor: &Value,
        requested_max_items: u32,
    ) -> Result<SourcePollBatch, ConnectorError> {
        self.check_cursor_size(cursor)?;
        let call = self.authorize(run_ctx, target, "poll")?;
        let bounded_max = requested_max_items.clamp(1, self.limits.max_batch_items);
        let batch = call
            .source()?
            .poll(&call.attempt, cursor, bounded_max)
            .await?;
        if batch.items.len() > bounded_max as usize {
            return Err(ConnectorError::contract_violation(
                "source_poll",
                format!(
                    "Source poll returned {} items, exceeding requested cap {bounded_max}",
                    batch.items.len()
                ),
                EffectStatus::Completed,
            ));
        }
        if let Some(ref next_cp) = batch.next_checkpoint {
            self.check_cursor_size(next_cp)?;
        }
        Ok(batch)
    }

    /// Authorize and execute `source.settle` for a single delivery ID.
    pub async fn settle_item(
        &self,
        run_ctx: &RunContext,
        target: SourceTarget,
        delivery_id: &str,
        settlement: SourceSettlement,
    ) -> Result<(), ConnectorError> {
        let call = self.authorize(run_ctx, target, "settle")?;
        call.source()?
            .settle(&call.attempt, delivery_id, settlement)
            .await
    }
}

/// The connection a source facet call acts on, the identity it acts as, and
/// the provider that serves it.
#[derive(Debug, Clone)]
pub struct SourceTarget {
    pub connection: ConnectionReference,
    pub effective_identity: EffectiveConnectionIdentity,
    pub kind: ImplementationKind,
    pub driver_id: String,
}

/// An authorized source facet call: the invocation (held for the call's
/// lifetime), its attempt, and the provider backend.
struct SourceCall {
    _invocation: super::context::Invocation,
    attempt: super::AttemptContext,
    backend: Arc<dyn super::ProviderBackend>,
    driver_id: String,
}

impl SourceCall {
    /// The provider's source facet, or a refusal when it has none.
    fn source(&self) -> Result<&dyn orion_extension_api::SourceConnectorFacet, ConnectorError> {
        self.backend.as_source().ok_or_else(|| {
            ConnectorError::forbidden(
                codes::OPERATION_FORBIDDEN,
                "dispatch",
                format!(
                    "Provider '{}' does not implement the source facet",
                    self.driver_id
                ),
                EffectStatus::NotStarted,
            )
        })
    }
}

#[cfg(test)]
mod tests;
