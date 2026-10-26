//! Native Rust `source` channel runner, fenced partition leases and contiguous checkpoint ledger (`V2-15.1`–`V2-15.4`).
//!
//! Implements the host-owned `source` channel execution lifecycle (`V2-15`):
//! - **Bounded Privileged Source Dispatch (`V2-15.1`)**: Only [`SourceChannelRunner`]
//!   can mint `orion.internal.source.*` invocations (`open`, `poll`, `settle`,
//!   `close`), enforcing batch-size and cursor-byte ceilings across all three
//!   connector backend kinds (`rust`, `workflow`, `wasm`).
//! - **Fenced Partition Ownership (`V2-15.2`)**: [`SourceLeaseCoordinator`]
//!   assigns monotonically increasing fence tokens per `(channel_id, partition_id)`
//!   in the `source_claims` table, with leases judged on the database clock on
//!   every backend. Stale owners after lease expiry or revision takeover cannot
//!   renew, acknowledge, or commit checkpoints.
//! - **Settlement Intent & Contiguous Checkpoints (`V2-15.3`)**:
//!   [`ContiguousCheckpointLedger`] records durable settlement intent before
//!   external ack/DLQ persistence, blocks checkpoint advancement across gaps or
//!   unpersisted DLQ items, and commits only contiguous settled offsets under
//!   the active fence token, to `source_checkpoints`.
//!
//! No channel protocol drives this yet: Community has no source provider (the
//! Kafka one moved to Orion Enterprise), so a `source` protocol would accept
//! channels nothing runs. An edition with a source provider registers the
//! channel driver.
//! - **Revision Handoff (`V2-15.4`)**: Transfers partition ownership to a new
//!   deployment revision with a higher fence token while enforcing checkpoint
//!   namespace compatibility.

use std::collections::BTreeMap;
use std::sync::Arc;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::context::RunContext;
use super::provider_backend::ProviderRegistry;
use super::{
    ConnectionReference, ConnectorError, EffectStatus, EffectiveConnectionIdentity,
    ImplementationKind, codes,
};
use super::{SourcePollBatch, SourceSettlement};

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
///
/// Carries no expiry: the lease's end is a `source_claims.lease_until`
/// timestamp on the database clock, and every check against it is made there.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SourcePartitionClaim {
    pub key: SourcePartitionKey,
    pub owner_id: String,
    pub revision: u64,
    pub checkpoint_namespace: String,
    pub fence_token: u64,
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

/// The code of a database refusal the transaction can simply be run again
/// after: a deadlock or a serialization failure between two claimants of one
/// partition (MySQL's `INSERT IGNORE` takes a shared lock on the duplicate,
/// and two claimants upgrading it deadlock).
const DB_RETRY: &str = "DB_RETRY";

fn db_error(context: &str, e: sqlx::Error) -> SourceCoordinationError {
    let retryable = e
        .as_database_error()
        .and_then(|d| d.code())
        .is_some_and(|code| matches!(code.as_ref(), "40001" | "40P01" | "1213"));
    SourceCoordinationError {
        code: if retryable { DB_RETRY } else { "DB_ERROR" },
        message: format!("{context}: {e}"),
    }
}

fn not_found(message: &str) -> SourceCoordinationError {
    SourceCoordinationError {
        code: "SOURCE_LEASE_NOT_FOUND",
        message: message.to_string(),
    }
}

/// What the row says about a claim, read back after a write.
type ClaimRow = (String, i64, String, i64);

/// Fenced partition leases and their checkpoints, held in the `source_claims`
/// and `source_checkpoints` tables and judged on the **database clock**
/// (`V2-15.2`, `V2-15.4`).
///
/// Every node sharing the state database contends on the same rows, so
/// ownership is a property of the database, not of a process: a claim is an
/// atomic conditional write, a lease is live while `lease_until` is ahead of
/// the database's `now`, and the fence token only ever increases — a takeover
/// or a revision handoff moves it, so a stale owner's renewals, settlements
/// and checkpoints are refused by the row they would have to match. No node's
/// clock enters any of it.
#[derive(Clone)]
pub struct SourceLeaseCoordinator {
    pool: crate::storage::DbPool,
}

impl std::fmt::Debug for SourceLeaseCoordinator {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SourceLeaseCoordinator")
            .field("backend", &self.pool.backend())
            .finish()
    }
}

impl SourceLeaseCoordinator {
    pub fn new(pool: crate::storage::DbPool) -> Self {
        Self { pool }
    }

    fn now(&self) -> sea_query::SimpleExpr {
        sea_query::Expr::cust(crate::storage::repositories::helpers::sql_now(
            self.pool.backend(),
        ))
    }

    fn until(&self, ttl_secs: u64) -> sea_query::SimpleExpr {
        sea_query::Expr::cust(crate::storage::repositories::helpers::sql_now_plus_secs(
            self.pool.backend(),
            ttl_secs.max(1),
        ))
    }

    fn key_cond(key: &SourcePartitionKey) -> sea_query::Condition {
        use crate::storage::schema::SourceClaims as C;
        use sea_query::{Expr, ExprTrait};
        sea_query::Condition::all()
            .add(Expr::col(C::ChannelId).eq(key.channel_id.as_str()))
            .add(Expr::col(C::PartitionId).eq(key.partition_id.as_str()))
    }

    async fn begin(&self) -> Result<crate::storage::DbTransaction, SourceCoordinationError> {
        self.pool
            .begin_write_tx()
            .await
            .map_err(|e| db_error("begin source_claims transaction", e))
    }

    /// The claim row for `key`, read inside `tx`.
    async fn read_claim(
        &self,
        tx: &mut crate::storage::DbTransaction,
        key: &SourcePartitionKey,
    ) -> Result<Option<SourcePartitionClaim>, SourceCoordinationError> {
        use crate::storage::schema::SourceClaims as C;
        let (sql, values) = crate::storage::build_sqlx(
            self.pool.backend(),
            sea_query::Query::select()
                .columns([
                    C::OwnerId,
                    C::Revision,
                    C::CheckpointNamespace,
                    C::FenceToken,
                ])
                .from(C::Table)
                .cond_where(Self::key_cond(key))
                .lock(sea_query::LockType::Update),
        );
        let row: Option<ClaimRow> = tx
            .fetch_optional_as(&sql, values)
            .await
            .map_err(|e| db_error("read source_claims", e))?;
        Ok(row.map(
            |(owner_id, revision, checkpoint_namespace, fence)| SourcePartitionClaim {
                key: key.clone(),
                owner_id,
                revision: revision.max(0) as u64,
                checkpoint_namespace,
                fence_token: fence.max(0) as u64,
            },
        ))
    }

    async fn commit(tx: crate::storage::DbTransaction) -> Result<(), SourceCoordinationError> {
        tx.commit()
            .await
            .map_err(|e| db_error("commit source_claims transaction", e))
    }

    /// Acquire an unclaimed partition, renew this owner's own claim, or take
    /// over one whose lease has expired on the database clock — each issuing
    /// a strictly greater `fence_token` (`V2-15.2`).
    pub async fn acquire_or_takeover(
        &self,
        key: SourcePartitionKey,
        owner_id: impl Into<String>,
        revision: u64,
        checkpoint_namespace: impl Into<String>,
        lease_ttl_secs: u64,
    ) -> Result<SourcePartitionClaim, SourceCoordinationError> {
        let owner_id = owner_id.into();
        let checkpoint_namespace = checkpoint_namespace.into();
        // Claimants of one partition contend by design; a deadlock between
        // two of them is a lost round, not a failure.
        let mut attempt = 0;
        loop {
            match self
                .acquire_once(
                    &key,
                    &owner_id,
                    revision,
                    &checkpoint_namespace,
                    lease_ttl_secs,
                )
                .await
            {
                Err(e) if e.code == DB_RETRY && attempt < 8 => attempt += 1,
                outcome => return outcome,
            }
        }
    }

    async fn acquire_once(
        &self,
        key: &SourcePartitionKey,
        owner_id: &str,
        revision: u64,
        checkpoint_namespace: &str,
        lease_ttl_secs: u64,
    ) -> Result<SourcePartitionClaim, SourceCoordinationError> {
        use crate::storage::schema::SourceClaims as C;
        use sea_query::{Expr, ExprTrait};
        let key = key.clone();
        let owner_id = owner_id.to_string();
        let checkpoint_namespace = checkpoint_namespace.to_string();
        let backend = self.pool.backend();
        let mut tx = self.begin().await?;

        // First claim of the partition: fence 1. A concurrent first claimant
        // that got there first makes this insert a no-op.
        let mut insert = sea_query::Query::insert()
            .into_table(C::Table)
            .columns([
                C::ChannelId,
                C::PartitionId,
                C::OwnerId,
                C::Revision,
                C::CheckpointNamespace,
                C::FenceToken,
                C::LeaseUntil,
            ])
            .values_panic([
                key.channel_id.as_str().into(),
                key.partition_id.as_str().into(),
                owner_id.as_str().into(),
                (revision as i64).into(),
                checkpoint_namespace.as_str().into(),
                1i64.into(),
                self.until(lease_ttl_secs),
            ])
            .to_owned();
        let inserted = match backend {
            crate::storage::DbBackend::Mysql => {
                let (sql, values) = crate::storage::build_sqlx(backend, &mut insert);
                let sql = sql.replacen("INSERT INTO", "INSERT IGNORE INTO", 1);
                tx.execute_query(&sql, values).await
            }
            _ => {
                insert.on_conflict(
                    sea_query::OnConflict::columns([C::ChannelId, C::PartitionId])
                        .do_nothing()
                        .to_owned(),
                );
                let (sql, values) = crate::storage::build_sqlx(backend, &mut insert);
                tx.execute_query(&sql, values).await
            }
        }
        .map_err(|e| db_error("insert source_claims", e))?;

        if inserted == 0 {
            // Held already. Take it when the lease is over, or when the
            // claimant is the holder itself (same owner, same revision); a
            // namespace change is a handoff, never a takeover.
            let (sql, values) = crate::storage::build_sqlx(
                backend,
                sea_query::Query::update()
                    .table(C::Table)
                    .value(C::OwnerId, owner_id.as_str())
                    .value(C::Revision, revision as i64)
                    .value(C::FenceToken, Expr::col(C::FenceToken).add(1))
                    .value(C::LeaseUntil, self.until(lease_ttl_secs))
                    .value(C::UpdatedAt, self.now())
                    .cond_where(
                        Self::key_cond(&key)
                            .add(
                                Expr::col(C::CheckpointNamespace).eq(checkpoint_namespace.as_str()),
                            )
                            .add(
                                sea_query::Condition::any()
                                    .add(Expr::col(C::LeaseUntil).lte(self.now()))
                                    .add(
                                        Expr::col(C::OwnerId)
                                            .eq(owner_id.as_str())
                                            .and(Expr::col(C::Revision).eq(revision as i64)),
                                    ),
                            ),
                    ),
            );
            let updated = tx
                .execute_query(&sql, values)
                .await
                .map_err(|e| db_error("take over source_claims", e))?;
            if updated == 0 {
                let current = self
                    .read_claim(&mut tx, &key)
                    .await?
                    .ok_or_else(|| not_found("source partition claim vanished during takeover"))?;
                return Err(if current.checkpoint_namespace != checkpoint_namespace {
                    SourceCoordinationError {
                        code: "SOURCE_CHECKPOINT_NAMESPACE_MISMATCH",
                        message: format!(
                            "Checkpoint namespace '{checkpoint_namespace}' is incompatible with active \
                             namespace '{}'; use explicit revision handoff",
                            current.checkpoint_namespace
                        ),
                    }
                } else {
                    SourceCoordinationError {
                        code: "SOURCE_PARTITION_LEASE_HELD",
                        message: format!(
                            "Partition '{}:{}' is leased by '{}' (rev {}), and the lease has not \
                             expired on the database clock",
                            key.channel_id, key.partition_id, current.owner_id, current.revision
                        ),
                    }
                });
            }
        }
        let claim = self
            .read_claim(&mut tx, &key)
            .await?
            .ok_or_else(|| not_found("source partition claim missing after write"))?;
        Self::commit(tx).await?;
        Ok(claim)
    }

    /// Extend a live lease, only for the owner holding the current fence; a
    /// lease already over on the database clock is not renewed — the owner
    /// must claim again, and gets a new fence (`V2-15.2`).
    pub async fn renew(
        &self,
        key: &SourcePartitionKey,
        owner_id: &str,
        fence_token: u64,
        lease_ttl_secs: u64,
    ) -> Result<SourcePartitionClaim, SourceCoordinationError> {
        use crate::storage::schema::SourceClaims as C;
        use sea_query::{Expr, ExprTrait};
        let mut tx = self.begin().await?;
        let (sql, values) = crate::storage::build_sqlx(
            self.pool.backend(),
            sea_query::Query::update()
                .table(C::Table)
                .value(C::LeaseUntil, self.until(lease_ttl_secs))
                .value(C::UpdatedAt, self.now())
                .cond_where(
                    Self::key_cond(key)
                        .add(Expr::col(C::OwnerId).eq(owner_id))
                        .add(Expr::col(C::FenceToken).eq(fence_token as i64))
                        .add(Expr::col(C::LeaseUntil).gt(self.now())),
                ),
        );
        let updated = tx
            .execute_query(&sql, values)
            .await
            .map_err(|e| db_error("renew source_claims", e))?;
        let current = self
            .read_claim(&mut tx, key)
            .await?
            .ok_or_else(|| not_found("No lease exists for source partition"))?;
        if updated == 0 {
            return Err(
                if current.fence_token != fence_token || current.owner_id != owner_id {
                    SourceCoordinationError {
                        code: "SOURCE_FENCE_STALE",
                        message: format!(
                            "Stale fence token {fence_token} (current fence is {})",
                            current.fence_token
                        ),
                    }
                } else {
                    SourceCoordinationError {
                        code: "SOURCE_LEASE_EXPIRED",
                        message: "Source partition lease expired before renewal".to_string(),
                    }
                },
            );
        }
        Self::commit(tx).await?;
        Ok(current)
    }

    /// Hand off a source partition to a new revision (`V2-15.4`).
    ///
    /// Increments the fence token so the old revision's runner cannot settle
    /// in-flight items or overwrite checkpoints after the handoff. Rejects a
    /// checkpoint namespace change unless `allow_incompatible_migration`.
    pub async fn handoff_revision(
        &self,
        key: &SourcePartitionKey,
        new_owner_id: impl Into<String>,
        new_revision: u64,
        new_checkpoint_namespace: impl Into<String>,
        allow_incompatible_migration: bool,
        lease_ttl_secs: u64,
    ) -> Result<SourcePartitionClaim, SourceCoordinationError> {
        use crate::storage::schema::SourceClaims as C;
        use sea_query::{Expr, ExprTrait};
        let new_owner_id = new_owner_id.into();
        let new_checkpoint_namespace = new_checkpoint_namespace.into();
        let mut tx = self.begin().await?;
        let Some(current) = self.read_claim(&mut tx, key).await? else {
            return Err(not_found("Cannot hand off an unclaimed source partition"));
        };
        if current.checkpoint_namespace != new_checkpoint_namespace && !allow_incompatible_migration
        {
            return Err(SourceCoordinationError {
                code: "SOURCE_CHECKPOINT_NAMESPACE_MISMATCH",
                message: format!(
                    "Incompatible checkpoint namespace handoff ('{}' -> '{}') requires explicit \
                     migration policy",
                    current.checkpoint_namespace, new_checkpoint_namespace
                ),
            });
        }
        let (sql, values) = crate::storage::build_sqlx(
            self.pool.backend(),
            sea_query::Query::update()
                .table(C::Table)
                .value(C::OwnerId, new_owner_id.as_str())
                .value(C::Revision, new_revision as i64)
                .value(C::CheckpointNamespace, new_checkpoint_namespace.as_str())
                .value(C::FenceToken, Expr::col(C::FenceToken).add(1))
                .value(C::LeaseUntil, self.until(lease_ttl_secs))
                .value(C::UpdatedAt, self.now())
                .cond_where(
                    Self::key_cond(key)
                        .add(Expr::col(C::FenceToken).eq(current.fence_token as i64)),
                ),
        );
        let updated = tx
            .execute_query(&sql, values)
            .await
            .map_err(|e| db_error("hand off source_claims", e))?;
        if updated == 0 {
            return Err(SourceCoordinationError {
                code: "SOURCE_FENCE_STALE",
                message: "The partition changed hands during the handoff".to_string(),
            });
        }
        let claim = self
            .read_claim(&mut tx, key)
            .await?
            .ok_or_else(|| not_found("source partition claim missing after handoff"))?;
        Self::commit(tx).await?;
        Ok(claim)
    }

    /// Verify that `fence_token` is the active fence for `key` and its lease
    /// is live on the database clock (`V2-15.2`).
    pub async fn validate_fence(
        &self,
        key: &SourcePartitionKey,
        fence_token: u64,
    ) -> Result<(), SourceCoordinationError> {
        let mut tx = self.begin().await?;
        let result = self.validate_fence_in(&mut tx, key, fence_token).await;
        Self::commit(tx).await?;
        result.map(|_| ())
    }

    /// [`Self::validate_fence`] inside `tx`, holding the claim row (on the
    /// backends with row locks) until `tx` ends.
    async fn validate_fence_in(
        &self,
        tx: &mut crate::storage::DbTransaction,
        key: &SourcePartitionKey,
        fence_token: u64,
    ) -> Result<SourcePartitionClaim, SourceCoordinationError> {
        use crate::storage::schema::SourceClaims as C;
        use sea_query::{Expr, ExprTrait};
        let Some(current) = self.read_claim(tx, key).await? else {
            return Err(not_found("No active lease for source partition"));
        };
        if current.fence_token != fence_token {
            return Err(SourceCoordinationError {
                code: "SOURCE_FENCE_STALE",
                message: format!(
                    "Operation rejected: stale fence token {fence_token} (active is {})",
                    current.fence_token
                ),
            });
        }
        let (sql, values) = crate::storage::build_sqlx(
            self.pool.backend(),
            sea_query::Query::select()
                .expr(sea_query::Func::count(Expr::col(C::FenceToken)))
                .from(C::Table)
                .cond_where(
                    Self::key_cond(key)
                        .add(Expr::col(C::FenceToken).eq(fence_token as i64))
                        .add(Expr::col(C::LeaseUntil).gt(self.now())),
                ),
        );
        let (live,): (i64,) = tx
            .fetch_optional_as(&sql, values)
            .await
            .map_err(|e| db_error("read source_claims", e))?
            .unwrap_or((0,));
        if live == 0 {
            return Err(SourceCoordinationError {
                code: "SOURCE_LEASE_EXPIRED",
                message: "Operation rejected: source partition lease has expired".to_string(),
            });
        }
        Ok(current)
    }

    /// Commit a contiguous checkpoint for `key` under `fence_token`, in one
    /// transaction with the fence check: a checkpoint lands only while its
    /// writer holds the current fence on a live lease (`V2-15.3`).
    pub async fn commit_checkpoint(
        &self,
        key: &SourcePartitionKey,
        fence_token: u64,
        contiguous_offset: u64,
        cursor: &Value,
    ) -> Result<(), SourceCoordinationError> {
        use crate::storage::schema::SourceCheckpoints as P;
        let mut tx = self.begin().await?;
        let claim = self.validate_fence_in(&mut tx, key, fence_token).await?;
        let cursor_json = serde_json::to_string(cursor).map_err(|e| SourceCoordinationError {
            code: "SERIALIZE_ERROR",
            message: format!("serialize source checkpoint: {e}"),
        })?;
        let (sql, values) = crate::storage::build_sqlx(
            self.pool.backend(),
            sea_query::Query::insert()
                .into_table(P::Table)
                .columns([
                    P::ChannelId,
                    P::PartitionId,
                    P::CheckpointNamespace,
                    P::FenceToken,
                    P::ContiguousOffset,
                    P::CursorJson,
                    P::UpdatedAt,
                ])
                .values_panic([
                    key.channel_id.as_str().into(),
                    key.partition_id.as_str().into(),
                    claim.checkpoint_namespace.as_str().into(),
                    (fence_token as i64).into(),
                    (contiguous_offset as i64).into(),
                    cursor_json.into(),
                    self.now(),
                ])
                .on_conflict(
                    sea_query::OnConflict::columns([
                        P::ChannelId,
                        P::PartitionId,
                        P::CheckpointNamespace,
                    ])
                    .update_columns([
                        P::FenceToken,
                        P::ContiguousOffset,
                        P::CursorJson,
                        P::UpdatedAt,
                    ])
                    .to_owned(),
                ),
        );
        tx.execute_query(&sql, values)
            .await
            .map_err(|e| db_error("upsert source_checkpoints", e))?;
        Self::commit(tx).await
    }

    /// The committed checkpoint for `key` in `checkpoint_namespace`: its
    /// contiguous offset, cursor, and the fence it was committed under.
    pub async fn load_checkpoint(
        &self,
        key: &SourcePartitionKey,
        checkpoint_namespace: &str,
    ) -> Result<Option<(u64, Value, u64)>, SourceCoordinationError> {
        use crate::storage::schema::SourceCheckpoints as P;
        use sea_query::{Expr, ExprTrait};
        let (sql, values) = crate::storage::build_sqlx(
            self.pool.backend(),
            sea_query::Query::select()
                .columns([P::ContiguousOffset, P::CursorJson, P::FenceToken])
                .from(P::Table)
                .and_where(Expr::col(P::ChannelId).eq(key.channel_id.as_str()))
                .and_where(Expr::col(P::PartitionId).eq(key.partition_id.as_str()))
                .and_where(Expr::col(P::CheckpointNamespace).eq(checkpoint_namespace)),
        );
        let row: Option<(i64, String, i64)> = self
            .pool
            .fetch_optional_as(&sql, values)
            .await
            .map_err(|e| db_error("read source_checkpoints", e))?;
        row.map(|(offset, cursor_json, fence)| {
            let cursor =
                serde_json::from_str(&cursor_json).map_err(|e| SourceCoordinationError {
                    code: "CORRUPT_PERSISTED_STATE",
                    message: format!("Corrupt cursor_json in source_checkpoints: {e}"),
                })?;
            Ok((
                Ord::max(offset, 0) as u64,
                cursor,
                Ord::max(fence, 0) as u64,
            ))
        })
        .transpose()
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

    /// Step 1 of settlement (`V2-15.3`): record settlement intent under the
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

    /// Step 2 of settlement (`V2-15.3`): confirm external settlement/DLQ
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
        }

        // How far the contiguous settled run reaches past the committed
        // sequence (`V2-15.3`).
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

    /// Authorize and execute `source.open` on a source-capable connector (`V2-15.1`).
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

    /// Authorize and execute `source.poll`, enforcing `max_batch_items` and cursor bounds (`V2-15.1`).
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

    /// Authorize and execute `source.settle` for a single delivery ID (`V2-15.1`, `V2-15.3`).
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
#[expect(clippy::unwrap_used, clippy::panic)]
mod tests {
    use super::*;
    use crate::extensions::AttemptContext;
    use crate::extensions::CompositionRoot;
    use crate::extensions::context::{AdmittedRun, AuthorityGrant, BudgetLimits};
    use crate::extensions::{ConnectorOutput, Digest, ProviderRef};
    use orion_extension_api::{ProviderBackend, SourceConnectorFacet};
    use serde_json::json;
    use std::collections::BTreeSet;
    use std::time::{Duration, Instant};

    const TEST_SOURCE: &str = "test.source";

    /// A source-capable provider that echoes its checkpoint and never yields
    /// an item: enough to drive the runner's privileged facet calls.
    struct EchoSource;

    #[async_trait::async_trait]
    impl ProviderBackend for EchoSource {
        fn kind(&self) -> ImplementationKind {
            ImplementationKind::Rust
        }

        fn driver_id(&self) -> &str {
            TEST_SOURCE
        }

        fn validate_config(&self, _config: &Value) -> Result<(), ConnectorError> {
            Ok(())
        }

        async fn invoke(
            &self,
            _ctx: &AttemptContext,
            _operation: &str,
            _input: Value,
        ) -> Result<ConnectorOutput, ConnectorError> {
            Ok(ConnectorOutput::new(json!({})))
        }

        fn as_source(&self) -> Option<&dyn SourceConnectorFacet> {
            Some(self)
        }
    }

    #[async_trait::async_trait]
    impl SourceConnectorFacet for EchoSource {
        async fn open(
            &self,
            _ctx: &AttemptContext,
            checkpoint: Option<Value>,
        ) -> Result<Value, ConnectorError> {
            Ok(json!({ "checkpoint": checkpoint }))
        }

        async fn poll(
            &self,
            _ctx: &AttemptContext,
            cursor: &Value,
            _max_items: u32,
        ) -> Result<SourcePollBatch, ConnectorError> {
            Ok(SourcePollBatch {
                items: Vec::new(),
                next_checkpoint: cursor.get("checkpoint").cloned(),
            })
        }

        async fn settle(
            &self,
            _ctx: &AttemptContext,
            _delivery_id: &str,
            _settlement: SourceSettlement,
        ) -> Result<(), ConnectorError> {
            Ok(())
        }

        async fn close(&self, _ctx: &AttemptContext, _cursor: Value) -> Result<(), ConnectorError> {
            Ok(())
        }
    }

    async fn coordinator() -> (SourceLeaseCoordinator, crate::storage::DbPool) {
        let pool = crate::storage::test_sqlite_pool().await;
        (SourceLeaseCoordinator::new(pool.clone()), pool)
    }

    /// Move a partition's lease into the database's past, as time would.
    async fn expire(pool: &crate::storage::DbPool, key: &SourcePartitionKey) {
        let crate::storage::DbPool::Sqlite(p) = pool else {
            panic!("sqlite expected");
        };
        sqlx::query(
            "UPDATE source_claims SET lease_until = datetime('now', '-1 seconds') \
             WHERE channel_id = ?1 AND partition_id = ?2",
        )
        .bind(&key.channel_id)
        .bind(&key.partition_id)
        .execute(p)
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn fenced_leases_contiguous_checkpoints_dlq_guard_and_revision_handoff() {
        let (coord, pool) = coordinator().await;
        let ledger = ContiguousCheckpointLedger::new(coord.clone());
        let key = SourcePartitionKey::new("orders-source", "part-0");

        // 1. Acquire initial lease on revision 1 (fence_token = 1).
        let claim_r1 = coord
            .acquire_or_takeover(key.clone(), "node-a", 1, "pg-lsn-v1", 60)
            .await
            .expect("acquired rev 1");
        assert_eq!(claim_r1.fence_token, 1);
        // Another owner cannot take a live lease.
        let held = coord
            .acquire_or_takeover(key.clone(), "node-b", 1, "pg-lsn-v1", 60)
            .await
            .expect_err("live lease");
        assert_eq!(held.code, "SOURCE_PARTITION_LEASE_HELD");

        // Enqueue 3 polled items (seq 1, 2, 3).
        let mut seqs = Vec::new();
        for (i, lsn) in [101, 102, 103].into_iter().enumerate() {
            seqs.push(
                ledger
                    .enqueue_polled_item(&key, 1, format!("deliv-{i}"), json!({"lsn": lsn}))
                    .await
                    .expect("enqueue"),
            );
        }
        assert_eq!(seqs, vec![1, 2, 3]);
        let (seq1, seq2, seq3) = (1, 2, 3);

        // 2. Out-of-order completion: seq 2 settles first while seq 1 is
        // still Pending. The gap at seq 1 blocks checkpoint advancement.
        ledger
            .record_settlement_intent(&key, 1, seq2, SourceSettlement::Ack)
            .await
            .expect("intent seq2");
        let cp = ledger
            .confirm_settled_and_advance(&key, 1, seq2, false)
            .await
            .expect("confirm seq2");
        assert!(
            cp.is_none(),
            "gap at seq 1 must block checkpoint advancement"
        );

        // 3. Seq 1 settles: contiguous advancement runs through 1 and 2, and
        // the checkpoint is in the database under fence 1.
        ledger
            .record_settlement_intent(&key, 1, seq1, SourceSettlement::Ack)
            .await
            .expect("intent seq1");
        let cp = ledger
            .confirm_settled_and_advance(&key, 1, seq1, false)
            .await
            .expect("confirm seq1");
        assert_eq!(cp, Some(json!({"lsn": 102})));
        assert_eq!(
            coord.load_checkpoint(&key, "pg-lsn-v1").await.unwrap(),
            Some((2, json!({"lsn": 102}), 1))
        );

        // 4. Seq 3 is DeadLetter but DLQ persistence failed: no advancement.
        ledger
            .record_settlement_intent(&key, 1, seq3, SourceSettlement::DeadLetter)
            .await
            .expect("intent seq3 dlq");
        let dlq_err = ledger
            .confirm_settled_and_advance(&key, 1, seq3, false)
            .await
            .expect_err("unpersisted DLQ must not advance checkpoint");
        assert_eq!(dlq_err.code, "SOURCE_DLQ_PERSISTENCE_REQUIRED");
        assert_eq!(
            ledger.committed_checkpoint(&key).await,
            Some((2, json!({"lsn": 102}), 1))
        );

        // 5. Revision handoff (`V2-15.4`): an incompatible namespace is
        // refused without the migration flag; a compatible one moves the
        // fence to 2 and fences out the old runner.
        let incompat = coord
            .handoff_revision(&key, "node-b", 2, "pg-lsn-v2-incompatible", false, 60)
            .await
            .expect_err("incompatible namespace must fail");
        assert_eq!(incompat.code, "SOURCE_CHECKPOINT_NAMESPACE_MISMATCH");
        let claim_r2 = coord
            .handoff_revision(&key, "node-b", 2, "pg-lsn-v1", false, 60)
            .await
            .expect("handoff to rev 2 succeeds");
        assert_eq!(claim_r2.fence_token, 2);

        let stale_err = ledger
            .confirm_settled_and_advance(&key, 1, seq3, true)
            .await
            .expect_err("stale fence 1 must be rejected after handoff");
        assert_eq!(stale_err.code, "SOURCE_FENCE_STALE");
        let stale_commit = coord
            .commit_checkpoint(&key, 1, 9, &json!({"lsn": 999}))
            .await
            .expect_err("a stale fence cannot write a checkpoint");
        assert_eq!(stale_commit.code, "SOURCE_FENCE_STALE");

        // Fence 2 settles seq 3 once the DLQ write succeeded.
        let cp_final = ledger
            .confirm_settled_and_advance(&key, 2, seq3, true)
            .await
            .expect("fence 2 settles seq 3");
        assert_eq!(cp_final, Some(json!({"lsn": 103})));
        assert_eq!(
            coord.load_checkpoint(&key, "pg-lsn-v1").await.unwrap(),
            Some((3, json!({"lsn": 103}), 2))
        );

        // A fresh ledger (a restarted runner) resumes from the database.
        let resumed = ContiguousCheckpointLedger::new(coord.clone());
        resumed.resume(&key, "pg-lsn-v1").await.unwrap();
        assert_eq!(
            resumed.committed_checkpoint(&key).await,
            Some((3, json!({"lsn": 103}), 2))
        );
        let next = resumed
            .enqueue_polled_item(&key, 2, "deliv-4", json!({"lsn": 104}))
            .await
            .unwrap();
        assert_eq!(next, 4, "sequence numbers continue after the checkpoint");
        drop(pool);
    }

    /// The lease is judged on the database clock: renewal works while the
    /// row's `lease_until` is ahead of the database's `now`, and once it is
    /// behind, the holder can neither renew nor act, and another node takes
    /// the partition over with a greater fence.
    #[tokio::test]
    async fn leases_expire_on_the_database_clock() {
        let (coord, pool) = coordinator().await;
        let key = SourcePartitionKey::new("orders-source", "part-1");
        let claim = coord
            .acquire_or_takeover(key.clone(), "node-a", 1, "ns", 60)
            .await
            .unwrap();
        coord
            .renew(&key, "node-a", claim.fence_token, 60)
            .await
            .unwrap();
        let stale = coord.renew(&key, "node-a", 99, 60).await.unwrap_err();
        assert_eq!(stale.code, "SOURCE_FENCE_STALE");
        // The holder re-acquiring its own live lease takes a new fence.
        let again = coord
            .acquire_or_takeover(key.clone(), "node-a", 1, "ns", 60)
            .await
            .unwrap();
        assert_eq!(again.fence_token, 2);

        expire(&pool, &key).await;
        assert_eq!(
            coord.renew(&key, "node-a", 2, 60).await.unwrap_err().code,
            "SOURCE_LEASE_EXPIRED"
        );
        assert_eq!(
            coord.validate_fence(&key, 2).await.unwrap_err().code,
            "SOURCE_LEASE_EXPIRED"
        );
        let takeover = coord
            .acquire_or_takeover(key.clone(), "node-b", 1, "ns", 60)
            .await
            .expect("an expired lease is taken over");
        assert_eq!(takeover.fence_token, 3);
        assert_eq!(takeover.owner_id, "node-b");
        // A takeover cannot change the checkpoint namespace.
        expire(&pool, &key).await;
        assert_eq!(
            coord
                .acquire_or_takeover(key.clone(), "node-c", 1, "other-ns", 60)
                .await
                .unwrap_err()
                .code,
            "SOURCE_CHECKPOINT_NAMESPACE_MISMATCH"
        );
        // Fences survive the process: a second coordinator over the same
        // database continues them.
        expire(&pool, &key).await;
        let other_process = SourceLeaseCoordinator::new(pool.clone());
        let next = other_process
            .acquire_or_takeover(key, "node-c", 1, "ns", 60)
            .await
            .unwrap();
        assert_eq!(next.fence_token, 4);
    }

    #[tokio::test]
    async fn source_runner_executes_privileged_facet_while_app_boundary_blocks_it() {
        let root = CompositionRoot::community()
            .with_provider_backend(Arc::new(EchoSource))
            .build_in_memory()
            .expect("composes");
        let runner =
            SourceChannelRunner::new(root.providers().clone(), SourceRunnerLimits::default());

        let provider = ProviderRef {
            publisher: "test".to_string(),
            provider_id: TEST_SOURCE.into(),
            version: "2.0.0".to_string(),
            digest: Digest::parse(&format!("sha256:{}", "ef".repeat(32))).unwrap(),
        };
        let conn = ConnectionReference {
            connection_id: "pg-outbox".into(),
            revision: crate::extensions::Revision::FIRST,
            provider,
        };
        let eff_id = EffectiveConnectionIdentity {
            provider_digest: conn.provider.digest.clone(),
            connection_id: "pg-outbox".into(),
            revision: crate::extensions::Revision::FIRST,
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

        // Privileged SourceChannelRunner succeeds in opening and polling the source facet.
        let cursor = runner
            .open_cursor(
                &run.context(),
                SourceTarget {
                    connection: conn.clone(),
                    effective_identity: eff_id.clone(),
                    kind: ImplementationKind::Rust,
                    driver_id: TEST_SOURCE.to_string(),
                },
                Some(json!({"lsn": 42})),
            )
            .await
            .expect("source open succeeds");
        assert_eq!(cursor["checkpoint"], json!({"lsn": 42}));

        let batch = runner
            .poll_batch(
                &run.context(),
                SourceTarget {
                    connection: conn,
                    effective_identity: eff_id,
                    kind: ImplementationKind::Rust,
                    driver_id: TEST_SOURCE.to_string(),
                },
                &cursor,
                25,
            )
            .await
            .expect("source poll succeeds");
        assert!(batch.items.is_empty());
        assert_eq!(batch.next_checkpoint, Some(json!({"lsn": 42})));
    }
}
