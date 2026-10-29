//! Settlement intent and contiguous checkpoint ledger (`ContiguousCheckpointLedger`).

use std::collections::BTreeMap;
use std::sync::Arc;

use serde_json::Value;

use super::SourceSettlement;
use super::lease::{SourceCoordinationError, SourceLeaseCoordinator, SourcePartitionKey};

/// Lifecycle state of a single polled source item in [`ContiguousCheckpointLedger`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ItemSettlementState {
    /// Item was polled and is awaiting settlement intent.
    Pending,
    /// Durable settlement intent was written prior to external ack or DLQ persistence.
    IntentRecorded(SourceSettlement),
    /// Item is terminally settled (`Ack` or persisted `DeadLetter`) and eligible for contiguous checkpoint advancement.
    Settled(SourceSettlement),
}

#[derive(Debug, Clone)]
struct TrackedItem {
    delivery_id: String,
    candidate_checkpoint: Value,
    state: ItemSettlementState,
}

#[derive(Debug, Default)]
struct PartitionLedger {
    next_seq: u64,
    committed_seq: u64,
    committed_checkpoint: Option<Value>,
    committed_fence: u64,
    items: BTreeMap<u64, TrackedItem>,
}

/// Crash-sensitive settlement and contiguous checkpoint ledger.
///
/// The in-flight items are the owning runner's own (they belong to the
/// process that polled them); the fence they are settled under and the
/// checkpoint they advance are the database's, through
/// [`SourceLeaseCoordinator`].
///
/// Guarantees:
/// 1. Settlement intent is recorded under a valid fence before external ack/DLQ I/O.
/// 2. `DeadLetter` items require `dlq_persisted == true` before they can settle;
///    a failed DLQ write preserves the item in `IntentRecorded` and blocks offset advancement.
/// 3. `NackRetryable` or out-of-order completions leave a gap that prevents
///    `committed_checkpoint` from advancing past the first unsettled sequence number.
/// 4. An advanced checkpoint is committed to `source_checkpoints` under the
///    active fence before the ledger treats it as committed.
#[derive(Debug, Clone)]
pub struct ContiguousCheckpointLedger {
    coordinator: SourceLeaseCoordinator,
    ledgers: Arc<tokio::sync::Mutex<BTreeMap<SourcePartitionKey, PartitionLedger>>>,
}

impl ContiguousCheckpointLedger {
    pub fn new(coordinator: SourceLeaseCoordinator) -> Self {
        Self {
            coordinator,
            ledgers: Arc::default(),
        }
    }

    /// Resume `key` from its committed checkpoint in `checkpoint_namespace`,
    /// if one is stored: sequence numbers continue after it. Answers it.
    pub async fn resume(
        &self,
        key: &SourcePartitionKey,
        checkpoint_namespace: &str,
    ) -> Result<Option<(u64, Value, u64)>, SourceCoordinationError> {
        let stored = self
            .coordinator
            .load_checkpoint(key, checkpoint_namespace)
            .await?;
        if let Some((offset, ref cursor, fence)) = stored {
            let mut guard = self.ledgers.lock().await;
            let ledger = guard.entry(key.clone()).or_default();
            ledger.committed_seq = offset;
            ledger.next_seq = ledger.next_seq.max(offset);
            ledger.committed_checkpoint = Some(cursor.clone());
            ledger.committed_fence = fence;
        }
        Ok(stored)
    }

    /// Register a polled item into the partition ledger under `fence_token`, returning its sequence number.
    pub async fn enqueue_polled_item(
        &self,
        key: &SourcePartitionKey,
        fence_token: u64,
        delivery_id: impl Into<String>,
        candidate_checkpoint: Value,
    ) -> Result<u64, SourceCoordinationError> {
        self.coordinator.validate_fence(key, fence_token).await?;
        let mut guard = self.ledgers.lock().await;
        let ledger = guard.entry(key.clone()).or_default();
        let seq = ledger.next_seq.saturating_add(1);
        ledger.next_seq = seq;
        ledger.items.insert(
            seq,
            TrackedItem {
                delivery_id: delivery_id.into(),
                candidate_checkpoint,
                state: ItemSettlementState::Pending,
            },
        );
        Ok(seq)
    }

    /// Step 1 of settlement: record settlement intent under the
    /// active fence before sending external `settle` or writing to DLQ.
    pub async fn record_settlement_intent(
        &self,
        key: &SourcePartitionKey,
        fence_token: u64,
        seq: u64,
        settlement: SourceSettlement,
    ) -> Result<String, SourceCoordinationError> {
        self.coordinator.validate_fence(key, fence_token).await?;
        let mut guard = self.ledgers.lock().await;
        let Some(ledger) = guard.get_mut(key) else {
            return Err(SourceCoordinationError {
                code: "SOURCE_LEDGER_NOT_FOUND",
                message: "Partition ledger not found".to_string(),
            });
        };
        let Some(item) = ledger.items.get_mut(&seq) else {
            return Err(SourceCoordinationError {
                code: "SOURCE_ITEM_NOT_FOUND",
                message: format!("Sequence {seq} not found in partition ledger"),
            });
        };
        item.state = ItemSettlementState::IntentRecorded(settlement);
        Ok(item.delivery_id.clone())
    }

    /// Step 2 of settlement: confirm external settlement/DLQ
    /// completion and advance the committed checkpoint only across contiguous
    /// settled sequence numbers, committing it under the fence first.
    pub async fn confirm_settled_and_advance(
        &self,
        key: &SourcePartitionKey,
        fence_token: u64,
        seq: u64,
        dlq_persisted: bool,
    ) -> Result<Option<Value>, SourceCoordinationError> {
        self.coordinator.validate_fence(key, fence_token).await?;
        // Held across the checkpoint write, so two confirmations on one
        // partition cannot commit out of order.
        let mut guard = self.ledgers.lock().await;
        let Some(ledger) = guard.get_mut(key) else {
            return Err(SourceCoordinationError {
                code: "SOURCE_LEDGER_NOT_FOUND",
                message: "Partition ledger not found".to_string(),
            });
        };
        let Some(item) = ledger.items.get_mut(&seq) else {
            return Err(SourceCoordinationError {
                code: "SOURCE_ITEM_NOT_FOUND",
                message: format!("Sequence {seq} not found in partition ledger"),
            });
        };

        let ItemSettlementState::IntentRecorded(disposition) = item.state else {
            return Err(SourceCoordinationError {
                code: "SOURCE_SETTLEMENT_INTENT_REQUIRED",
                message: format!(
                    "Cannot confirm settlement for seq {seq} before recording durable intent"
                ),
            });
        };

        match disposition {
            SourceSettlement::Ack => {
                item.state = ItemSettlementState::Settled(SourceSettlement::Ack);
            }
            SourceSettlement::DeadLetter => {
                if !dlq_persisted {
                    return Err(SourceCoordinationError {
                        code: "SOURCE_DLQ_PERSISTENCE_REQUIRED",
                        message: "DLQ persistence failed; payload preserved and checkpoint advancement forbidden".to_string(),
                    });
                }
                item.state = ItemSettlementState::Settled(SourceSettlement::DeadLetter);
            }
            SourceSettlement::NackRetryable => {
                // Retryable nack leaves the sequence unsettled so it blocks contiguous advancement.
                return Ok(ledger.committed_checkpoint.clone());
            }
            // A disposition added later is not an acknowledgement: leave the
            // sequence unsettled, as for a retryable nack.
            _ => return Ok(ledger.committed_checkpoint.clone()),
        }

        // How far the contiguous settled run reaches past the committed
        // sequence.
        let mut reach = ledger.committed_seq;
        while ledger
            .items
            .get(&reach.saturating_add(1))
            .is_some_and(|next| matches!(next.state, ItemSettlementState::Settled(_)))
        {
            reach = reach.saturating_add(1);
        }
        if reach == ledger.committed_seq {
            return Ok(ledger.committed_checkpoint.clone());
        }
        let checkpoint = ledger.items[&reach].candidate_checkpoint.clone();
        self.coordinator
            .commit_checkpoint(key, fence_token, reach, &checkpoint)
            .await?;
        for settled in ledger.committed_seq.saturating_add(1)..=reach {
            ledger.items.remove(&settled);
        }
        ledger.committed_seq = reach;
        ledger.committed_checkpoint = Some(checkpoint);
        ledger.committed_fence = fence_token;
        Ok(ledger.committed_checkpoint.clone())
    }

    pub async fn committed_checkpoint(
        &self,
        key: &SourcePartitionKey,
    ) -> Option<(u64, Value, u64)> {
        let guard = self.ledgers.lock().await;
        let ledger = guard.get(key)?;
        Some((
            ledger.committed_seq,
            ledger.committed_checkpoint.clone()?,
            ledger.committed_fence,
        ))
    }
}
