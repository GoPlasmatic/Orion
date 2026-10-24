//! Native Rust `source` channel runner, fenced partition leases and contiguous checkpoint ledger (`V2-15.1`–`V2-15.4`).
//!
//! Implements the host-owned `source` channel execution lifecycle (`V2-15`):
//! - **Bounded Privileged Source Dispatch (`V2-15.1`)**: Only [`SourceChannelRunner`]
//!   can mint `orion.internal.source.*` invocations (`open`, `poll`, `settle`,
//!   `close`), enforcing batch-size and cursor-byte ceilings across all three
//!   connector backend kinds (`rust`, `workflow`, `wasm`).
//! - **Fenced Partition Ownership (`V2-15.2`)**: [`SourceLeaseCoordinator`]
//!   assigns monotonically increasing fence tokens per `(channel_id, partition_id)`.
//!   Stale owners after lease expiry or revision takeover cannot renew, acknowledge,
//!   or commit checkpoints.
//! - **Settlement Intent & Contiguous Checkpoints (`V2-15.3`)**:
//!   [`ContiguousCheckpointLedger`] records durable settlement intent before
//!   external ack/DLQ persistence, blocks checkpoint advancement across gaps or
//!   unpersisted DLQ items, and commits only contiguous settled offsets under
//!   the active fence token.
//! - **Revision Handoff (`V2-15.4`)**: Transfers partition ownership to a new
//!   deployment revision with a higher fence token while enforcing checkpoint
//!   namespace compatibility.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::admission::AdmissionPolicy;
use super::context::RunContext;
use super::contract::{
    ConnectionReference, ConnectorError, EffectStatus, EffectiveConnectionIdentity,
    ImplementationKind, codes,
};
use super::provider_backend::{ProviderRegistry, SourcePollBatch, SourceSettlement};

/// Stable source partition key across revisions (`V2-15.2`).
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct SourcePartitionKey {
    pub channel_id: String,
    pub partition_id: String,
}

impl SourcePartitionKey {
    pub fn new(channel_id: impl Into<String>, partition_id: impl Into<String>) -> Self {
        Self {
            channel_id: channel_id.into(),
            partition_id: partition_id.into(),
        }
    }
}

/// Active fenced ownership claim for a source partition (`V2-15.2`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SourcePartitionClaim {
    pub key: SourcePartitionKey,
    pub owner_id: String,
    pub revision: u64,
    pub checkpoint_namespace: String,
    pub fence_token: u64,
    pub expires_at_ms: u64,
}

/// Error returned when a source lease, fence, or checkpoint invariant is violated (`V2-15`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SourceCoordinationError {
    pub code: &'static str,
    pub message: String,
}

impl std::fmt::Display for SourceCoordinationError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.code, self.message)
    }
}

impl std::error::Error for SourceCoordinationError {}

#[derive(Debug)]
struct PartitionSlot {
    claim: SourcePartitionClaim,
    highest_fence: u64,
}

/// Coordinator managing DB-time lease claims and monotonic fence tokens (`V2-15.2`, `V2-15.4`).
#[derive(Debug, Default, Clone)]
pub struct SourceLeaseCoordinator {
    slots: Arc<Mutex<BTreeMap<SourcePartitionKey, PartitionSlot>>>,
}

impl SourceLeaseCoordinator {
    pub fn new() -> Self {
        Self::default()
    }

    /// Acquire an unclaimed partition or take over an expired lease, issuing a
    /// strictly increasing `fence_token` (`V2-15.2`).
    pub fn acquire_or_takeover(
        &self,
        key: SourcePartitionKey,
        owner_id: impl Into<String>,
        revision: u64,
        checkpoint_namespace: impl Into<String>,
        now_ms: u64,
        lease_ttl_ms: u64,
    ) -> Result<SourcePartitionClaim, SourceCoordinationError> {
        let owner_id = owner_id.into();
        let checkpoint_namespace = checkpoint_namespace.into();
        let mut guard = self.slots.lock().unwrap_or_else(|e| e.into_inner());

        if let Some(slot) = guard.get_mut(&key) {
            let expired = now_ms >= slot.claim.expires_at_ms;
            let same_owner = slot.claim.owner_id == owner_id && slot.claim.revision == revision;
            if !expired && !same_owner {
                return Err(SourceCoordinationError {
                    code: "SOURCE_PARTITION_LEASE_HELD",
                    message: format!(
                        "Partition '{}:{}' is currently leased by '{}' (rev {}) until {}ms",
                        key.channel_id,
                        key.partition_id,
                        slot.claim.owner_id,
                        slot.claim.revision,
                        slot.claim.expires_at_ms
                    ),
                });
            }
            if slot.claim.checkpoint_namespace != checkpoint_namespace {
                return Err(SourceCoordinationError {
                    code: "SOURCE_CHECKPOINT_NAMESPACE_MISMATCH",
                    message: format!(
                        "Checkpoint namespace '{}' is incompatible with active namespace '{}'; use explicit revision handoff",
                        checkpoint_namespace, slot.claim.checkpoint_namespace
                    ),
                });
            }

            let next_fence = slot.highest_fence.saturating_add(1);
            slot.highest_fence = next_fence;
            slot.claim = SourcePartitionClaim {
                key,
                owner_id,
                revision,
                checkpoint_namespace,
                fence_token: next_fence,
                expires_at_ms: now_ms.saturating_add(lease_ttl_ms.max(1)),
            };
            return Ok(slot.claim.clone());
        }

        let claim = SourcePartitionClaim {
            key: key.clone(),
            owner_id,
            revision,
            checkpoint_namespace,
            fence_token: 1,
            expires_at_ms: now_ms.saturating_add(lease_ttl_ms.max(1)),
        };
        guard.insert(
            key,
            PartitionSlot {
                claim: claim.clone(),
                highest_fence: 1,
            },
        );
        Ok(claim)
    }

    /// Renew an active lease only if `owner_id` and `fence_token` match and the
    /// lease has not been taken over (`V2-15.2`).
    pub fn renew(
        &self,
        key: &SourcePartitionKey,
        owner_id: &str,
        fence_token: u64,
        now_ms: u64,
        lease_ttl_ms: u64,
    ) -> Result<SourcePartitionClaim, SourceCoordinationError> {
        let mut guard = self.slots.lock().unwrap_or_else(|e| e.into_inner());
        let Some(slot) = guard.get_mut(key) else {
            return Err(SourceCoordinationError {
                code: "SOURCE_LEASE_NOT_FOUND",
                message: "No lease exists for source partition".to_string(),
            });
        };
        if slot.claim.fence_token != fence_token || slot.claim.owner_id != owner_id {
            return Err(SourceCoordinationError {
                code: "SOURCE_FENCE_STALE",
                message: format!(
                    "Stale fence token {fence_token} (current fence is {})",
                    slot.claim.fence_token
                ),
            });
        }
        if now_ms >= slot.claim.expires_at_ms {
            return Err(SourceCoordinationError {
                code: "SOURCE_LEASE_EXPIRED",
                message: "Source partition lease expired before renewal".to_string(),
            });
        }
        slot.claim.expires_at_ms = now_ms.saturating_add(lease_ttl_ms.max(1));
        Ok(slot.claim.clone())
    }

    /// Hand off a source partition to a new revision (`V2-15.4`).
    ///
    /// Increments the monotonic fence token so the old revision runner cannot
    /// settle in-flight items or overwrite checkpoints after handoff. Rejects
    /// checkpoint namespace changes unless `allow_incompatible_migration` is true.
    #[allow(clippy::too_many_arguments)]
    pub fn handoff_revision(
        &self,
        key: &SourcePartitionKey,
        new_owner_id: impl Into<String>,
        new_revision: u64,
        new_checkpoint_namespace: impl Into<String>,
        allow_incompatible_migration: bool,
        now_ms: u64,
        lease_ttl_ms: u64,
    ) -> Result<SourcePartitionClaim, SourceCoordinationError> {
        let new_owner_id = new_owner_id.into();
        let new_checkpoint_namespace = new_checkpoint_namespace.into();
        let mut guard = self.slots.lock().unwrap_or_else(|e| e.into_inner());
        let Some(slot) = guard.get_mut(key) else {
            return Err(SourceCoordinationError {
                code: "SOURCE_LEASE_NOT_FOUND",
                message: "Cannot hand off an unclaimed source partition".to_string(),
            });
        };

        if slot.claim.checkpoint_namespace != new_checkpoint_namespace
            && !allow_incompatible_migration
        {
            return Err(SourceCoordinationError {
                code: "SOURCE_CHECKPOINT_NAMESPACE_MISMATCH",
                message: format!(
                    "Incompatible checkpoint namespace handoff ('{}' -> '{}') requires explicit migration policy",
                    slot.claim.checkpoint_namespace, new_checkpoint_namespace
                ),
            });
        }

        let next_fence = slot.highest_fence.saturating_add(1);
        slot.highest_fence = next_fence;
        slot.claim = SourcePartitionClaim {
            key: key.clone(),
            owner_id: new_owner_id,
            revision: new_revision,
            checkpoint_namespace: new_checkpoint_namespace,
            fence_token: next_fence,
            expires_at_ms: now_ms.saturating_add(lease_ttl_ms.max(1)),
        };
        Ok(slot.claim.clone())
    }

    /// Verify that `fence_token` is the active, unexpired fence for `key` (`V2-15.2`).
    pub fn validate_fence(
        &self,
        key: &SourcePartitionKey,
        fence_token: u64,
        now_ms: u64,
    ) -> Result<(), SourceCoordinationError> {
        let guard = self.slots.lock().unwrap_or_else(|e| e.into_inner());
        let Some(slot) = guard.get(key) else {
            return Err(SourceCoordinationError {
                code: "SOURCE_LEASE_NOT_FOUND",
                message: "No active lease for source partition".to_string(),
            });
        };
        if slot.claim.fence_token != fence_token {
            return Err(SourceCoordinationError {
                code: "SOURCE_FENCE_STALE",
                message: format!(
                    "Operation rejected: stale fence token {fence_token} (active is {})",
                    slot.claim.fence_token
                ),
            });
        }
        if now_ms >= slot.claim.expires_at_ms {
            return Err(SourceCoordinationError {
                code: "SOURCE_LEASE_EXPIRED",
                message: "Operation rejected: source partition lease has expired".to_string(),
            });
        }
        Ok(())
    }

    /// Persist all active partition claims and fence tokens to the `source_claims` SQLite table (`TD-27`).
    pub async fn persist_to_sqlite(
        &self,
        pool: &sqlx::SqlitePool,
    ) -> Result<(), SourceCoordinationError> {
        let claims: Vec<SourcePartitionClaim> = {
            let guard = self.slots.lock().unwrap_or_else(|e| e.into_inner());
            guard.values().map(|s| s.claim.clone()).collect()
        };
        let mut tx = pool.begin().await.map_err(|e| SourceCoordinationError {
            code: "DB_ERROR",
            message: format!("Failed to begin SQLite transaction for source_claims: {e}"),
        })?;
        for c in claims {
            sqlx::query(
                "INSERT INTO source_claims \
                 (channel_id, partition_id, owner_id, revision, checkpoint_namespace, fence_token, lease_until) \
                 VALUES (?, ?, ?, ?, ?, ?, ?) \
                 ON CONFLICT(channel_id, partition_id) DO UPDATE SET \
                 owner_id = excluded.owner_id, \
                 revision = excluded.revision, \
                 checkpoint_namespace = excluded.checkpoint_namespace, \
                 fence_token = excluded.fence_token, \
                 lease_until = excluded.lease_until, \
                 updated_at = CURRENT_TIMESTAMP",
            )
            .bind(&c.key.channel_id)
            .bind(&c.key.partition_id)
            .bind(&c.owner_id)
            .bind(c.revision as i64)
            .bind(&c.checkpoint_namespace)
            .bind(c.fence_token as i64)
            .bind(c.expires_at_ms.to_string())
            .execute(&mut *tx)
            .await
            .map_err(|e| SourceCoordinationError {
                code: "DB_ERROR",
                message: format!("Failed to upsert source_claims row: {e}"),
            })?;
        }
        tx.commit().await.map_err(|e| SourceCoordinationError {
            code: "DB_ERROR",
            message: format!("Failed to commit source_claims transaction: {e}"),
        })?;
        Ok(())
    }

    /// Hydrate partition claims and fence tokens from the `source_claims` SQLite table (`TD-27`).
    pub async fn load_from_sqlite(
        &self,
        pool: &sqlx::SqlitePool,
    ) -> Result<usize, SourceCoordinationError> {
        use sqlx::Row;
        let rows = sqlx::query(
            "SELECT channel_id, partition_id, owner_id, revision, checkpoint_namespace, fence_token, lease_until \
             FROM source_claims",
        )
        .fetch_all(pool)
        .await
        .map_err(|e| SourceCoordinationError {
            code: "DB_ERROR",
            message: format!("Failed to query source_claims: {e}"),
        })?;

        let mut guard = self.slots.lock().unwrap_or_else(|e| e.into_inner());
        let mut count = 0usize;
        for r in rows {
            let channel_id: String = r.get("channel_id");
            let partition_id: String = r.get("partition_id");
            let owner_id: String = r.get("owner_id");
            let revision = r.get::<i64, _>("revision") as u64;
            let checkpoint_namespace: String = r.get("checkpoint_namespace");
            let fence_token = r.get::<i64, _>("fence_token") as u64;
            let lease_until_str: String = r.try_get("lease_until").unwrap_or_default();
            let expires_at_ms = lease_until_str.parse::<u64>().unwrap_or(0);
            let key = SourcePartitionKey::new(channel_id, partition_id);
            let claim = SourcePartitionClaim {
                key: key.clone(),
                owner_id,
                revision,
                checkpoint_namespace,
                fence_token,
                expires_at_ms,
            };
            let highest_fence = guard
                .get(&key)
                .map(|s| s.highest_fence.max(fence_token))
                .unwrap_or(fence_token);
            guard.insert(
                key,
                PartitionSlot {
                    claim,
                    highest_fence,
                },
            );
            count += 1;
        }
        Ok(count)
    }
}

/// Lifecycle state of a single polled source item in [`ContiguousCheckpointLedger`] (`V2-15.3`).
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

/// Crash-sensitive settlement and contiguous checkpoint ledger (`V2-15.3`).
///
/// Guarantees:
/// 1. Settlement intent is recorded under a valid fence before external ack/DLQ I/O.
/// 2. `DeadLetter` items require `dlq_persisted == true` before they can settle;
///    a failed DLQ write preserves the item in `IntentRecorded` and blocks offset advancement.
/// 3. `NackRetryable` or out-of-order completions leave a gap that prevents
///    `committed_checkpoint` from advancing past the first unsettled sequence number.
#[derive(Debug, Clone)]
pub struct ContiguousCheckpointLedger {
    coordinator: SourceLeaseCoordinator,
    ledgers: Arc<Mutex<BTreeMap<SourcePartitionKey, PartitionLedger>>>,
}

impl ContiguousCheckpointLedger {
    pub fn new(coordinator: SourceLeaseCoordinator) -> Self {
        Self {
            coordinator,
            ledgers: Arc::new(Mutex::new(BTreeMap::new())),
        }
    }

    /// Register a polled item into the partition ledger under `fence_token`, returning its sequence number.
    pub fn enqueue_polled_item(
        &self,
        key: &SourcePartitionKey,
        fence_token: u64,
        now_ms: u64,
        delivery_id: impl Into<String>,
        candidate_checkpoint: Value,
    ) -> Result<u64, SourceCoordinationError> {
        self.coordinator.validate_fence(key, fence_token, now_ms)?;
        let mut guard = self.ledgers.lock().unwrap_or_else(|e| e.into_inner());
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

    /// Step 1 of settlement (`V2-15.3`): persist durable settlement intent under active fence
    /// before sending external `settle` or writing to DLQ.
    pub fn record_settlement_intent(
        &self,
        key: &SourcePartitionKey,
        fence_token: u64,
        now_ms: u64,
        seq: u64,
        settlement: SourceSettlement,
    ) -> Result<String, SourceCoordinationError> {
        self.coordinator.validate_fence(key, fence_token, now_ms)?;
        let mut guard = self.ledgers.lock().unwrap_or_else(|e| e.into_inner());
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

    /// Step 2 of settlement (`V2-15.3`): confirm external settlement/DLQ completion and
    /// advance the committed checkpoint only across contiguous settled sequence numbers.
    pub fn confirm_settled_and_advance(
        &self,
        key: &SourcePartitionKey,
        fence_token: u64,
        now_ms: u64,
        seq: u64,
        dlq_persisted: bool,
    ) -> Result<Option<Value>, SourceCoordinationError> {
        self.coordinator.validate_fence(key, fence_token, now_ms)?;
        let mut guard = self.ledgers.lock().unwrap_or_else(|e| e.into_inner());
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
        }

        // Advance committed_seq and committed_checkpoint only while the next sequence
        // is contiguously Settled (`V2-15.3`).
        loop {
            let candidate_seq = ledger.committed_seq.saturating_add(1);
            let Some(next_item) = ledger.items.get(&candidate_seq) else {
                break;
            };
            if matches!(next_item.state, ItemSettlementState::Settled(_)) {
                ledger.committed_seq = candidate_seq;
                ledger.committed_checkpoint = Some(next_item.candidate_checkpoint.clone());
                ledger.committed_fence = fence_token;
                ledger.items.remove(&candidate_seq);
            } else {
                break;
            }
        }

        Ok(ledger.committed_checkpoint.clone())
    }

    pub fn committed_checkpoint(&self, key: &SourcePartitionKey) -> Option<(u64, Value, u64)> {
        let guard = self.ledgers.lock().unwrap_or_else(|e| e.into_inner());
        let ledger = guard.get(key)?;
        Some((
            ledger.committed_seq,
            ledger.committed_checkpoint.clone()?,
            ledger.committed_fence,
        ))
    }

    /// Persist all committed contiguous checkpoints to the `source_checkpoints` SQLite table (`TD-27`).
    pub async fn persist_to_sqlite(
        &self,
        pool: &sqlx::SqlitePool,
        default_namespace: &str,
    ) -> Result<(), SourceCoordinationError> {
        let entries: Vec<(SourcePartitionKey, String, u64, u64, String)> = {
            let slots = self
                .coordinator
                .slots
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            let guard = self.ledgers.lock().unwrap_or_else(|e| e.into_inner());
            let mut out = Vec::new();
            for (k, l) in guard.iter() {
                if let Some(ref cp) = l.committed_checkpoint {
                    let ns = slots
                        .get(k)
                        .map(|s| s.claim.checkpoint_namespace.clone())
                        .unwrap_or_else(|| default_namespace.to_string());
                    let cursor_json =
                        serde_json::to_string(cp).unwrap_or_else(|_| "{}".to_string());
                    out.push((
                        k.clone(),
                        ns,
                        l.committed_fence,
                        l.committed_seq,
                        cursor_json,
                    ));
                }
            }
            out
        };

        let mut tx = pool.begin().await.map_err(|e| SourceCoordinationError {
            code: "DB_ERROR",
            message: format!("Failed to begin SQLite transaction for source_checkpoints: {e}"),
        })?;
        for (key, ns, fence, seq, cursor_json) in entries {
            sqlx::query(
                "INSERT INTO source_checkpoints \
                 (channel_id, partition_id, checkpoint_namespace, fence_token, contiguous_offset, cursor_json) \
                 VALUES (?, ?, ?, ?, ?, ?) \
                 ON CONFLICT(channel_id, partition_id, checkpoint_namespace) DO UPDATE SET \
                 fence_token = excluded.fence_token, \
                 contiguous_offset = excluded.contiguous_offset, \
                 cursor_json = excluded.cursor_json, \
                 updated_at = CURRENT_TIMESTAMP \
                 WHERE excluded.fence_token >= source_checkpoints.fence_token",
            )
            .bind(&key.channel_id)
            .bind(&key.partition_id)
            .bind(&ns)
            .bind(fence as i64)
            .bind(seq as i64)
            .bind(&cursor_json)
            .execute(&mut *tx)
            .await
            .map_err(|e| SourceCoordinationError {
                code: "DB_ERROR",
                message: format!("Failed to upsert source_checkpoints row: {e}"),
            })?;
        }
        tx.commit().await.map_err(|e| SourceCoordinationError {
            code: "DB_ERROR",
            message: format!("Failed to commit source_checkpoints transaction: {e}"),
        })?;
        Ok(())
    }

    /// Hydrate committed contiguous checkpoints from the `source_checkpoints` SQLite table (`TD-27`).
    pub async fn load_from_sqlite(
        &self,
        pool: &sqlx::SqlitePool,
    ) -> Result<usize, SourceCoordinationError> {
        use sqlx::Row;
        let rows = sqlx::query(
            "SELECT channel_id, partition_id, fence_token, contiguous_offset, cursor_json \
             FROM source_checkpoints",
        )
        .fetch_all(pool)
        .await
        .map_err(|e| SourceCoordinationError {
            code: "DB_ERROR",
            message: format!("Failed to query source_checkpoints: {e}"),
        })?;

        let mut guard = self.ledgers.lock().unwrap_or_else(|e| e.into_inner());
        let mut count = 0usize;
        for r in rows {
            let channel_id: String = r.get("channel_id");
            let partition_id: String = r.get("partition_id");
            let fence_token = r.get::<i64, _>("fence_token") as u64;
            let contiguous_offset = r.get::<i64, _>("contiguous_offset") as u64;
            let cursor_json: String = r.get("cursor_json");
            let cp: Value = serde_json::from_str(&cursor_json).map_err(|e| {
                SourceCoordinationError {
                    code: "CORRUPT_PERSISTED_STATE",
                    message: format!("Corrupt cursor_json in source_checkpoints: {e}"),
                }
            })?;
            let key = SourcePartitionKey::new(channel_id, partition_id);
            let ledger = guard.entry(key).or_default();
            ledger.committed_seq = contiguous_offset;
            ledger.next_seq = ledger.next_seq.max(contiguous_offset);
            ledger.committed_checkpoint = Some(cp);
            ledger.committed_fence = fence_token;
            count += 1;
        }
        Ok(count)
    }
}

/// Bounds enforced by [`SourceChannelRunner`] on every source facet call (`V2-15.1`).
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

/// Host-owned native Rust `source` channel runner (`V2-15.1`–`V2-15.4`).
///
/// Drives any registered connector (`rust`, `workflow`, or `wasm`) that exposes
/// [`super::provider_backend::SourceConnectorFacet`].
pub struct SourceChannelRunner {
    providers: ProviderRegistry,
    admission: Arc<dyn AdmissionPolicy>,
    limits: SourceRunnerLimits,
}

impl SourceChannelRunner {
    pub fn new(
        providers: ProviderRegistry,
        admission: Arc<dyn AdmissionPolicy>,
        limits: SourceRunnerLimits,
    ) -> Self {
        Self {
            providers,
            admission,
            limits,
        }
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

    /// Authorize and execute `source.open` on a source-capable connector (`V2-15.1`).
    pub async fn open_cursor(
        &self,
        run_ctx: &RunContext,
        connection: ConnectionReference,
        effective_identity: EffectiveConnectionIdentity,
        kind: ImplementationKind,
        driver_id: &str,
        checkpoint: Option<Value>,
    ) -> Result<Value, ConnectorError> {
        self.admission.check_runtime_lease(
            run_ctx.generation_id(),
            &connection.provider.provider_id,
            &connection.connection_id,
            EffectStatus::NotStarted,
        )?;
        let inv = run_ctx.start_privileged_source_invocation(
            connection,
            effective_identity,
            "open",
            1,
        )?;
        let attempt = inv.next_attempt()?;
        let backend = self.providers.get(kind, driver_id).ok_or_else(|| {
            ConnectorError::forbidden(
                codes::CONNECTION_FORBIDDEN,
                "dispatch",
                format!("Source provider ({kind}, '{driver_id}') is not registered"),
                EffectStatus::NotStarted,
            )
        })?;
        let source = backend.as_source().ok_or_else(|| {
            ConnectorError::forbidden(
                codes::OPERATION_FORBIDDEN,
                "dispatch",
                format!("Provider '{driver_id}' does not implement the source facet"),
                EffectStatus::NotStarted,
            )
        })?;
        let cursor = source.open(&attempt, checkpoint).await?;
        self.check_cursor_size(&cursor)?;
        Ok(cursor)
    }

    /// Authorize and execute `source.poll`, enforcing `max_batch_items` and cursor bounds (`V2-15.1`).
    #[allow(clippy::too_many_arguments)]
    pub async fn poll_batch(
        &self,
        run_ctx: &RunContext,
        connection: ConnectionReference,
        effective_identity: EffectiveConnectionIdentity,
        kind: ImplementationKind,
        driver_id: &str,
        cursor: &Value,
        requested_max_items: u32,
    ) -> Result<SourcePollBatch, ConnectorError> {
        self.check_cursor_size(cursor)?;
        self.admission.check_runtime_lease(
            run_ctx.generation_id(),
            &connection.provider.provider_id,
            &connection.connection_id,
            EffectStatus::NotStarted,
        )?;
        let inv = run_ctx.start_privileged_source_invocation(
            connection,
            effective_identity,
            "poll",
            1,
        )?;
        let attempt = inv.next_attempt()?;
        let backend = self.providers.get(kind, driver_id).ok_or_else(|| {
            ConnectorError::forbidden(
                codes::CONNECTION_FORBIDDEN,
                "dispatch",
                format!("Source provider ({kind}, '{driver_id}') is not registered"),
                EffectStatus::NotStarted,
            )
        })?;
        let source = backend.as_source().ok_or_else(|| {
            ConnectorError::forbidden(
                codes::OPERATION_FORBIDDEN,
                "dispatch",
                format!("Provider '{driver_id}' does not implement the source facet"),
                EffectStatus::NotStarted,
            )
        })?;
        let bounded_max = requested_max_items.clamp(1, self.limits.max_batch_items);
        let batch = source.poll(&attempt, cursor, bounded_max).await?;
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

    /// Authorize and execute `source.settle` for a single delivery ID (`V2-15.1`, `V2-15.3`).
    #[allow(clippy::too_many_arguments)]
    pub async fn settle_item(
        &self,
        run_ctx: &RunContext,
        connection: ConnectionReference,
        effective_identity: EffectiveConnectionIdentity,
        kind: ImplementationKind,
        driver_id: &str,
        delivery_id: &str,
        settlement: SourceSettlement,
    ) -> Result<(), ConnectorError> {
        self.admission.check_runtime_lease(
            run_ctx.generation_id(),
            &connection.provider.provider_id,
            &connection.connection_id,
            EffectStatus::NotStarted,
        )?;
        let inv = run_ctx.start_privileged_source_invocation(
            connection,
            effective_identity,
            "settle",
            1,
        )?;
        let attempt = inv.next_attempt()?;
        let backend = self.providers.get(kind, driver_id).ok_or_else(|| {
            ConnectorError::forbidden(
                codes::CONNECTION_FORBIDDEN,
                "dispatch",
                format!("Source provider ({kind}, '{driver_id}') is not registered"),
                EffectStatus::NotStarted,
            )
        })?;
        let source = backend.as_source().ok_or_else(|| {
            ConnectorError::forbidden(
                codes::OPERATION_FORBIDDEN,
                "dispatch",
                format!("Provider '{driver_id}' does not implement the source facet"),
                EffectStatus::NotStarted,
            )
        })?;
        source.settle(&attempt, delivery_id, settlement).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::extensions::CompositionRoot;
    use crate::extensions::context::{AdmittedRun, AuthorityGrant, BudgetLimits};
    use crate::extensions::contract::ProviderReference;
    use crate::extensions::foundation::DRIVER_POSTGRESQL;
    use serde_json::json;
    use std::collections::BTreeSet;
    use std::time::{Duration, Instant};

    #[test]
    fn v2_15_2_3_4_fenced_leases_contiguous_checkpoints_dlq_guard_and_revision_handoff() {
        let coord = SourceLeaseCoordinator::new();
        let ledger = ContiguousCheckpointLedger::new(coord.clone());
        let key = SourcePartitionKey::new("orders-source", "part-0");

        // 1. Acquire initial lease on revision 1 (fence_token = 1).
        let claim_r1 = coord
            .acquire_or_takeover(key.clone(), "node-a", 1, "pg-lsn-v1", 1_000, 5_000)
            .expect("acquired rev 1");
        assert_eq!(claim_r1.fence_token, 1);

        // Enqueue 3 polled items (seq 1, 2, 3).
        let seq1 = ledger
            .enqueue_polled_item(&key, 1, 1_100, "deliv-1", json!({"lsn": 101}))
            .expect("seq1");
        let seq2 = ledger
            .enqueue_polled_item(&key, 1, 1_100, "deliv-2", json!({"lsn": 102}))
            .expect("seq2");
        let seq3 = ledger
            .enqueue_polled_item(&key, 1, 1_100, "deliv-3", json!({"lsn": 103}))
            .expect("seq3");
        assert_eq!((seq1, seq2, seq3), (1, 2, 3));

        // 2. Out-of-order completion: seq 2 settles first while seq 1 is still Pending.
        // Gap at seq 1 must block checkpoint advancement!
        ledger
            .record_settlement_intent(&key, 1, 1_200, seq2, SourceSettlement::Ack)
            .expect("intent seq2");
        let cp = ledger
            .confirm_settled_and_advance(&key, 1, 1_200, seq2, false)
            .expect("confirm seq2");
        assert!(
            cp.is_none(),
            "gap at seq 1 must block checkpoint advancement"
        );

        // 3. Seq 1 settles -> contiguous advancement jumps through both seq 1 and seq 2 to lsn 102!
        ledger
            .record_settlement_intent(&key, 1, 1_300, seq1, SourceSettlement::Ack)
            .expect("intent seq1");
        let cp = ledger
            .confirm_settled_and_advance(&key, 1, 1_300, seq1, false)
            .expect("confirm seq1");
        assert_eq!(cp, Some(json!({"lsn": 102})));

        // 4. Seq 3 is marked DeadLetter, but DLQ persistence fails (`dlq_persisted = false`).
        // Must reject confirmation and keep checkpoint at 102!
        ledger
            .record_settlement_intent(&key, 1, 1_400, seq3, SourceSettlement::DeadLetter)
            .expect("intent seq3 dlq");
        let dlq_err = ledger
            .confirm_settled_and_advance(&key, 1, 1_400, seq3, false)
            .expect_err("unpersisted DLQ must not advance checkpoint");
        assert_eq!(dlq_err.code, "SOURCE_DLQ_PERSISTENCE_REQUIRED");
        assert_eq!(
            ledger.committed_checkpoint(&key),
            Some((2, json!({"lsn": 102}), 1))
        );

        // 5. Revision handoff (`V2-15.4`): incompatible namespace rejected without migration flag;
        // compatible namespace advances fence to 2 and fences old runner (fence 1).
        let incompat = coord
            .handoff_revision(
                &key,
                "node-b",
                2,
                "pg-lsn-v2-incompatible",
                false,
                1_500,
                5_000,
            )
            .expect_err("incompatible namespace must fail");
        assert_eq!(incompat.code, "SOURCE_CHECKPOINT_NAMESPACE_MISMATCH");

        let claim_r2 = coord
            .handoff_revision(&key, "node-b", 2, "pg-lsn-v1", false, 1_500, 5_000)
            .expect("handoff to rev 2 succeeds");
        assert_eq!(claim_r2.fence_token, 2);

        // Old runner with fence 1 can no longer confirm seq 3!
        let stale_err = ledger
            .confirm_settled_and_advance(&key, 1, 1_600, seq3, true)
            .expect_err("stale fence 1 must be rejected after handoff");
        assert_eq!(stale_err.code, "SOURCE_FENCE_STALE");

        // New runner with fence 2 confirms seq 3 after DLQ persistence succeeds -> checkpoint advances to 103.
        let cp_final = ledger
            .confirm_settled_and_advance(&key, 2, 1_600, seq3, true)
            .expect("fence 2 settles seq 3");
        assert_eq!(cp_final, Some(json!({"lsn": 103})));
    }

    #[tokio::test]
    async fn v2_15_1_source_runner_executes_privileged_facet_while_app_boundary_blocks_it() {
        let root = CompositionRoot::community();
        let runner = SourceChannelRunner::new(
            root.providers.clone(),
            root.admission.clone(),
            SourceRunnerLimits::default(),
        );

        let provider = ProviderReference {
            provider_id: DRIVER_POSTGRESQL.to_string(),
            version: "2.0.0".to_string(),
            package_digest:
                "sha256:efefefefefefefefefefefefefefefefefefefefefefefefefefefefefefefef"
                    .to_string(),
        };
        let conn = ConnectionReference {
            connection_id: "pg-outbox".to_string(),
            revision: 1,
            provider,
        };
        let eff_id = EffectiveConnectionIdentity {
            provider_digest: conn.provider.package_digest.clone(),
            connection_id: "pg-outbox".to_string(),
            revision: 1,
            secret_hash: "sha256:5555".to_string(),
        };

        let run = AdmittedRun::admit(
            "run-source",
            1,
            Instant::now() + Duration::from_secs(30),
            BudgetLimits::default(),
            AuthorityGrant {
                allowed_operations: BTreeMap::from([(
                    "pg-outbox".to_string(),
                    BTreeSet::from(["query".to_string()]),
                )]),
                declared_dependencies: BTreeMap::new(),
                allowed_channels: BTreeSet::new(),
            },
        );

        // Application boundary must reject reserved `orion.internal.source.poll`.
        let app_err = run
            .context()
            .start_invocation(
                conn.clone(),
                eff_id.clone(),
                "orion.internal.source.poll",
                None,
                1,
            )
            .expect_err("app boundary must block reserved source operation");
        assert_eq!(app_err.code, codes::OPERATION_FORBIDDEN);

        // Privileged SourceChannelRunner succeeds in opening and polling the PostgreSQL source facet!
        let cursor = runner
            .open_cursor(
                &run.context(),
                conn.clone(),
                eff_id.clone(),
                ImplementationKind::Rust,
                DRIVER_POSTGRESQL,
                Some(json!({"lsn": 42})),
            )
            .await
            .expect("source open succeeds");
        assert_eq!(cursor["checkpoint"], json!({"lsn": 42}));

        let batch = runner
            .poll_batch(
                &run.context(),
                conn,
                eff_id,
                ImplementationKind::Rust,
                DRIVER_POSTGRESQL,
                &cursor,
                25,
            )
            .await
            .expect("source poll succeeds");
        assert!(batch.items.is_empty());
        assert_eq!(batch.next_checkpoint, Some(json!({"lsn": 42})));
    }

    #[tokio::test]
    async fn td27_source_claims_and_checkpoints_persist_to_sqlite() {
        let pool = sqlx::sqlite::SqlitePoolOptions::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await
            .expect("sqlite memory pool");
        sqlx::raw_sql(include_str!(
            "../../migrations/sqlite/023_v2_immutable_deployments.sql"
        ))
        .execute(&pool)
        .await
        .expect("023 migration");

        let coord = SourceLeaseCoordinator::new();
        let ledger = ContiguousCheckpointLedger::new(coord.clone());
        let key = SourcePartitionKey::new("orders-source", "part-0");

        let claim = coord
            .acquire_or_takeover(key.clone(), "node-a", 1, "pg-lsn-v1", 1_000, 5_000)
            .expect("acquire claim");
        assert_eq!(claim.fence_token, 1);

        let seq1 = ledger
            .enqueue_polled_item(&key, 1, 1_100, "deliv-1", json!({"lsn": 777}))
            .expect("enqueue");
        ledger
            .record_settlement_intent(&key, 1, 1_200, seq1, SourceSettlement::Ack)
            .expect("intent");
        ledger
            .confirm_settled_and_advance(&key, 1, 1_200, seq1, false)
            .expect("confirm");

        coord
            .persist_to_sqlite(&pool)
            .await
            .expect("persist claims");
        ledger
            .persist_to_sqlite(&pool, "pg-lsn-v1")
            .await
            .expect("persist checkpoints");

        // Hydrate into a fresh coordinator and ledger
        let coord2 = SourceLeaseCoordinator::new();
        let ledger2 = ContiguousCheckpointLedger::new(coord2.clone());
        assert_eq!(coord2.load_from_sqlite(&pool).await.expect("load claims"), 1);
        assert_eq!(
            ledger2
                .load_from_sqlite(&pool)
                .await
                .expect("load checkpoints"),
            1
        );

        // Next takeover on coord2 must issue fence_token = 2 (preserving monotonic fence across restarts!)
        let takeover = coord2
            .acquire_or_takeover(key.clone(), "node-b", 1, "pg-lsn-v1", 10_000, 5_000)
            .expect("takeover after expiry");
        assert_eq!(takeover.fence_token, 2);
        assert_eq!(
            ledger2.committed_checkpoint(&key),
            Some((1, json!({"lsn": 777}), 1))
        );
    }
}
