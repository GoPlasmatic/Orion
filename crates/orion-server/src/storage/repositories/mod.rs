pub mod audit_logs;
pub mod channels;
pub mod cluster;
pub mod connectors;
pub mod helpers;
pub mod packages;
pub mod trace_dlq;
pub mod traces;
pub(crate) mod versioned;
pub mod workflows;

use std::sync::Arc;

use crate::errors::OrionError;
use audit_logs::AuditEvent;

/// The repository set backing `AppState` and the background tasks, all
/// constructed from the same startup pool.
///
/// Also the `repos` group on `AppStateInner` (R26): bootstrap builds one and
/// moves it into state wholesale, so the state's repository set and the one
/// handed to background tasks can never drift apart.
pub struct Repositories {
    /// The startup pool, kept so a route can span one entity write and its
    /// audit row in a single transaction ([`Repositories::audited`]).
    pool: crate::storage::DbPool,
    pub workflows: Arc<dyn workflows::WorkflowRepository>,
    pub channels: Arc<dyn channels::ChannelRepository>,
    pub connectors: Arc<dyn connectors::ConnectorRepository>,
    pub traces: Arc<dyn traces::TraceRepository>,
    pub audit_logs: Arc<dyn audit_logs::AuditLogRepository>,
    pub trace_dlq: Arc<dyn trace_dlq::TraceDlqRepository>,
    pub packages: Arc<dyn packages::PackageRepository>,
}

impl Repositories {
    /// Create repositories. `storage` supplies the optional at-rest cipher
    /// for connector configs (H3) — validated at config load, so a bad key
    /// never reaches this point.
    pub fn new(
        pool: &crate::storage::DbPool,
        storage: &crate::config::StorageConfig,
    ) -> Result<Self, crate::errors::OrionError> {
        let cipher = if storage.connector_encryption_key.is_empty() {
            None
        } else {
            Some(Arc::new(
                crate::storage::config_encryption::ConfigCipher::from_hex(
                    &storage.connector_encryption_key,
                )?,
            ))
        };
        Ok(Self {
            pool: pool.clone(),
            workflows: Arc::new(workflows::SqlWorkflowRepository::new(pool.clone())),
            channels: Arc::new(channels::SqlChannelRepository::new(pool.clone())),
            connectors: Arc::new(connectors::SqlConnectorRepository::with_cipher(
                pool.clone(),
                cipher,
            )),
            traces: Arc::new(traces::SqlTraceRepository::new(pool.clone())),
            audit_logs: Arc::new(audit_logs::SqlAuditLogRepository::new(pool.clone())),
            trace_dlq: Arc::new(trace_dlq::SqlTraceDlqRepository::new(pool.clone())),
            packages: Arc::new(packages::SqlPackageRepository::new(pool.clone())),
        })
    }

    /// Begin a mutation whose audit row commits with it (§2.6).
    ///
    /// Cross-repository consistency used to be by convention: the entity write
    /// committed, then the audit row went onto a bounded queue and was written
    /// by another task on another connection. Between those two points the
    /// change is live and unrecorded, and it stays that way if the process
    /// exits, if the queue is full, or if the audit INSERT fails — the states
    /// `orion_audit_events_dropped_total` counts. An audit trail with holes in
    /// it exactly where a change succeeded is the one shape an audit trail
    /// must not have.
    ///
    /// So the two writes share a transaction. `write` runs against the
    /// transaction this returns, [`AuditedWrite::commit`] adds the audit row
    /// and commits both, and dropping the guard without committing rolls the
    /// entity write back — which is the other half of the guarantee: a
    /// mutation whose audit row cannot be written does not happen.
    ///
    /// The queue is still there and still drained; it is the sink for audit
    /// events that have **no** entity write to join — `test`, `reload`,
    /// `backup`, and the bulk imports that span many rows. Those cannot lose a
    /// record of a live change, because they do not make one.
    ///
    /// [`crate::storage::DbPool::begin_write_tx`] rather than `begin_tx`: the
    /// lifecycle writes read before they write (D30).
    pub async fn audited(&self, event: AuditEvent) -> Result<AuditedWrite<'_>, OrionError> {
        Ok(AuditedWrite {
            tx: self.pool.begin_write_tx().await?,
            audit_logs: self.audit_logs.as_ref(),
            event,
        })
    }
}

/// One entity mutation and its audit row, in one transaction — see
/// [`Repositories::audited`].
///
/// Not `Drop`-based: sqlx rolls a transaction back when it is dropped without
/// a commit, so the failure path needs no code and cannot be forgotten. What
/// this type adds is that the audit row is written by [`Self::commit`] and
/// nowhere else, so there is no way to commit the entity write alone.
pub struct AuditedWrite<'a> {
    tx: crate::storage::DbTransaction,
    audit_logs: &'a dyn audit_logs::AuditLogRepository,
    event: AuditEvent,
}

impl AuditedWrite<'_> {
    /// The transaction the entity write must run in. A write that runs
    /// anywhere else is not covered by this guard.
    pub fn tx(&mut self) -> &mut crate::storage::DbTransaction {
        &mut self.tx
    }

    /// Write the audit row and commit both.
    ///
    /// A failure here rolls the entity write back with it, which is the
    /// intended trade: refusing a mutation is recoverable — the caller sees an
    /// error and retries — while accepting one silently unrecorded is not.
    pub async fn commit(mut self) -> Result<(), OrionError> {
        self.audit_logs.insert_tx(&mut self.tx, &self.event).await?;
        self.tx.commit().await?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::models::EntityStatus;

    async fn repos() -> (crate::storage::DbPool, Repositories) {
        let pool = crate::storage::test_sqlite_pool().await;
        let repos = Repositories::new(&pool, &crate::config::StorageConfig::default())
            .expect("repositories");
        (pool, repos)
    }

    fn event(action: &str, resource_id: &str) -> AuditEvent {
        AuditEvent {
            principal: "tester".to_string(),
            action: action.to_string(),
            resource_type: "channel".to_string(),
            resource_id: resource_id.to_string(),
            details: None,
        }
    }

    async fn seed_active_channel(repos: &Repositories, id: &str) {
        let req = serde_json::from_value(serde_json::json!({
            "channel_id": id,
            "name": id,
            "channel_type": "sync",
            "protocol": "rest",
            "route_pattern": format!("/{id}"),
            "methods": ["POST"],
        }))
        .expect("request");
        repos.channels.create(&req).await.expect("create");
        repos.channels.activate(id).await.expect("activate");
    }

    async fn audit_rows(repos: &Repositories) -> i64 {
        repos
            .audit_logs
            .list_paginated(&audit_logs::AuditLogFilter::default())
            .await
            .expect("list")
            .total
    }

    /// §2.6: the entity write and its audit row are one commit. Both are
    /// visible the moment `commit` returns — no queue, nothing to poll.
    #[tokio::test]
    async fn a_committed_audited_write_lands_both_rows() {
        let (_pool, repos) = repos().await;
        seed_active_channel(&repos, "chan-commit").await;
        assert_eq!(audit_rows(&repos).await, 0);

        let mut write = repos
            .audited(event("status_archived", "chan-commit"))
            .await
            .expect("begin");
        let archived = repos
            .channels
            .archive_tx(write.tx(), "chan-commit")
            .await
            .expect("archive");
        write.commit().await.expect("commit");

        assert_eq!(archived.status, EntityStatus::Archived.as_str());
        assert_eq!(
            repos
                .channels
                .get_by_id("chan-commit")
                .await
                .expect("read back")
                .status,
            EntityStatus::Archived.as_str()
        );
        assert_eq!(
            audit_rows(&repos).await,
            1,
            "the audit row must be committed with the change, not queued behind it"
        );
    }

    /// The other half, and the reason the guard exists: a mutation whose audit
    /// row is never written does not happen either.
    ///
    /// Dropping the guard is how every `?` between `audited` and `commit`
    /// leaves it, so this is the failure path of all five routes, not a
    /// hypothetical one.
    #[tokio::test]
    async fn dropping_an_audited_write_rolls_the_entity_write_back() {
        let (_pool, repos) = repos().await;
        seed_active_channel(&repos, "chan-rollback").await;

        {
            let mut write = repos
                .audited(event("delete", "chan-rollback"))
                .await
                .expect("begin");
            repos
                .channels
                .delete_tx(write.tx(), "chan-rollback")
                .await
                .expect("delete");
            // No `commit`: the guard goes out of scope here, exactly as it
            // would on an early return.
        }

        assert!(
            repos.channels.get_by_id("chan-rollback").await.is_ok(),
            "an audited write that was never committed must leave the channel in place"
        );
        assert_eq!(
            audit_rows(&repos).await,
            0,
            "and must leave no audit row behind either"
        );
    }
}
