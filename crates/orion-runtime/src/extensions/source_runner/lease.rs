//! Fenced partition ownership and checkpoint persistence in `source_claims` and `source_checkpoints`.

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Stable source partition key across revisions.
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

/// Active fenced ownership claim for a source partition.
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

/// Error returned when a source lease, fence, or checkpoint invariant is violated.
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
/// and `source_checkpoints` tables and judged on the **database clock**.
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
    /// a strictly greater `fence_token`.
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
    /// must claim again, and gets a new fence.
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

    /// Hand off a source partition to a new revision.
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
    /// is live on the database clock.
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
    /// writer holds the current fence on a live lease.
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
