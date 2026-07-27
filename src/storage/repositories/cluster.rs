//! Cluster coordination repository: config epoch, breaker-reset fan-out,
//! and job leases (multi-instance-ha A2/A7/A11).
//!
//! All statements use each backend's own clock (`datetime('now')` /
//! `LOCALTIMESTAMP` / `UTC_TIMESTAMP()`) so lease comparisons never depend
//! on node clocks — only the DB's.

use async_trait::async_trait;
use sea_query::{Value, Values};
use sea_query_binder::SqlxValues;

use crate::errors::OrionError;
use crate::storage::{DbBackend, DbPool, get_backend};

/// The single `config_epoch` row.
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct EpochRow {
    pub epoch: i64,
    pub breaker_epoch: i64,
    pub breaker_key: String,
}

#[async_trait]
pub trait ClusterRepository: Send + Sync {
    /// Atomically increment the config epoch; returns the post-increment value.
    async fn bump_epoch(&self) -> Result<i64, OrionError>;

    /// Read the current epoch row.
    async fn get_epoch(&self) -> Result<EpochRow, OrionError>;

    /// Bump `breaker_epoch` and record the breaker key so other nodes reset
    /// it locally. Returns the new breaker epoch.
    async fn request_breaker_reset(&self, key: &str) -> Result<i64, OrionError>;

    /// Acquire or renew the lease on `job_name` for `holder` for `ttl_secs`.
    /// Returns `true` when `holder` holds the lease after the call. The
    /// incumbent always renews; a foreign lease is only taken over once
    /// expired.
    async fn try_acquire_job_lease(
        &self,
        job_name: &str,
        holder: &str,
        ttl_secs: u64,
    ) -> Result<bool, OrionError>;
}

pub struct SqlClusterRepository {
    pool: DbPool,
}

impl SqlClusterRepository {
    pub fn new(pool: DbPool) -> Self {
        Self { pool }
    }

    fn no_values() -> SqlxValues {
        SqlxValues(Values(Vec::new()))
    }

    fn missing_epoch_row() -> OrionError {
        OrionError::Internal(
            "config_epoch row missing — cluster_coordination migration not applied".to_string(),
        )
    }
}

#[async_trait]
impl ClusterRepository for SqlClusterRepository {
    async fn bump_epoch(&self) -> Result<i64, OrionError> {
        crate::metrics::timed_db_op("cluster.bump_epoch", async {
            let now = super::helpers::sql_now(get_backend());
            match get_backend() {
                DbBackend::Sqlite | DbBackend::Postgres => {
                    let sql = format!(
                        "UPDATE config_epoch SET epoch = epoch + 1, updated_at = {now} \
                         WHERE id = 1 RETURNING epoch"
                    );
                    self.pool
                        .fetch_scalar::<i64>(&sql, Self::no_values())
                        .await
                        .map_err(OrionError::Storage)
                }
                DbBackend::Mysql => {
                    // MySQL has no UPDATE ... RETURNING; read back inside the
                    // same transaction (the row lock makes it our increment).
                    let mut tx = self.pool.begin_tx().await.map_err(OrionError::Storage)?;
                    tx.execute_query(
                        &format!(
                            "UPDATE config_epoch SET epoch = epoch + 1, updated_at = {now} \
                             WHERE id = 1"
                        ),
                        Self::no_values(),
                    )
                    .await?;
                    let (epoch,): (i64,) = super::helpers::fetch_required_tx(
                        &mut tx,
                        "SELECT epoch FROM config_epoch WHERE id = 1",
                        Self::no_values(),
                        Self::missing_epoch_row,
                    )
                    .await?;
                    tx.commit().await.map_err(OrionError::Storage)?;
                    Ok(epoch)
                }
            }
        })
        .await
    }

    async fn get_epoch(&self) -> Result<EpochRow, OrionError> {
        crate::metrics::timed_db_op("cluster.get_epoch", async {
            super::helpers::fetch_required(
                &self.pool,
                "SELECT epoch, breaker_epoch, breaker_key FROM config_epoch WHERE id = 1",
                Self::no_values(),
                Self::missing_epoch_row,
            )
            .await
        })
        .await
    }

    async fn request_breaker_reset(&self, key: &str) -> Result<i64, OrionError> {
        crate::metrics::timed_db_op("cluster.request_breaker_reset", async {
            let now = super::helpers::sql_now(get_backend());
            match get_backend() {
                DbBackend::Sqlite | DbBackend::Postgres => {
                    let ph = if get_backend() == DbBackend::Postgres {
                        "$1"
                    } else {
                        "?"
                    };
                    let sql = format!(
                        "UPDATE config_epoch \
                         SET breaker_epoch = breaker_epoch + 1, breaker_key = {ph}, \
                             updated_at = {now} \
                         WHERE id = 1 RETURNING breaker_epoch"
                    );
                    self.pool
                        .fetch_scalar::<i64>(&sql, SqlxValues(Values(vec![Value::from(key)])))
                        .await
                        .map_err(OrionError::Storage)
                }
                DbBackend::Mysql => {
                    let mut tx = self.pool.begin_tx().await.map_err(OrionError::Storage)?;
                    tx.execute_query(
                        &format!(
                            "UPDATE config_epoch \
                             SET breaker_epoch = breaker_epoch + 1, breaker_key = ?, \
                                 updated_at = {now} \
                             WHERE id = 1"
                        ),
                        SqlxValues(Values(vec![Value::from(key)])),
                    )
                    .await?;
                    let (breaker_epoch,): (i64,) = super::helpers::fetch_required_tx(
                        &mut tx,
                        "SELECT breaker_epoch FROM config_epoch WHERE id = 1",
                        Self::no_values(),
                        Self::missing_epoch_row,
                    )
                    .await?;
                    tx.commit().await.map_err(OrionError::Storage)?;
                    Ok(breaker_epoch)
                }
            }
        })
        .await
    }

    async fn try_acquire_job_lease(
        &self,
        job_name: &str,
        holder: &str,
        ttl_secs: u64,
    ) -> Result<bool, OrionError> {
        crate::metrics::timed_db_op("cluster.try_acquire_job_lease", async {
            let backend = get_backend();
            let now = super::helpers::sql_now(backend);
            let expiry = super::helpers::sql_now_plus_secs(backend, ttl_secs);

            // Step 1: renew own lease or take over an expired one.
            let (update_sql, update_values): (String, SqlxValues) = match backend {
                DbBackend::Sqlite | DbBackend::Mysql => (
                    format!(
                        "UPDATE job_leases SET holder = ?, expires_at = {expiry} \
                         WHERE job_name = ? AND (holder = ? OR expires_at < {now})"
                    ),
                    SqlxValues(Values(vec![
                        Value::from(holder),
                        Value::from(job_name),
                        Value::from(holder),
                    ])),
                ),
                DbBackend::Postgres => (
                    format!(
                        "UPDATE job_leases SET holder = $1, expires_at = {expiry} \
                         WHERE job_name = $2 AND (holder = $1 OR expires_at < {now})"
                    ),
                    SqlxValues(Values(vec![Value::from(holder), Value::from(job_name)])),
                ),
            };
            if self.pool.execute_query(&update_sql, update_values).await? > 0 {
                return Ok(true);
            }

            // Step 2: no row (first acquisition) — insert-if-absent. A loss
            // to a concurrent inserter means another node holds the lease.
            let insert_sql = match backend {
                DbBackend::Sqlite => format!(
                    "INSERT OR IGNORE INTO job_leases (job_name, holder, expires_at) \
                     VALUES (?, ?, {expiry})"
                ),
                DbBackend::Postgres => format!(
                    "INSERT INTO job_leases (job_name, holder, expires_at) \
                     VALUES ($1, $2, {expiry}) ON CONFLICT (job_name) DO NOTHING"
                ),
                DbBackend::Mysql => format!(
                    "INSERT IGNORE INTO job_leases (job_name, holder, expires_at) \
                     VALUES (?, ?, {expiry})"
                ),
            };
            let insert_values =
                SqlxValues(Values(vec![Value::from(job_name), Value::from(holder)]));
            Ok(self.pool.execute_query(&insert_sql, insert_values).await? > 0)
        })
        .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn test_repo() -> SqlClusterRepository {
        SqlClusterRepository::new(crate::storage::test_sqlite_pool().await)
    }

    #[tokio::test]
    async fn test_bump_epoch_increments() {
        let repo = test_repo().await;
        assert_eq!(repo.get_epoch().await.expect("get").epoch, 0);
        assert_eq!(repo.bump_epoch().await.expect("bump"), 1);
        assert_eq!(repo.bump_epoch().await.expect("bump"), 2);
        assert_eq!(repo.get_epoch().await.expect("get").epoch, 2);
    }

    #[tokio::test]
    async fn test_breaker_reset_records_key() {
        let repo = test_repo().await;
        assert_eq!(
            repo.request_breaker_reset("conn:http")
                .await
                .expect("reset"),
            1
        );
        let row = repo.get_epoch().await.expect("get");
        assert_eq!(row.breaker_epoch, 1);
        assert_eq!(row.breaker_key, "conn:http");
        // Config epoch untouched.
        assert_eq!(row.epoch, 0);
    }

    #[tokio::test]
    async fn test_job_lease_acquire_renew_and_contention() {
        let repo = test_repo().await;
        // First acquisition.
        assert!(
            repo.try_acquire_job_lease("trace_cleanup", "node-a", 60)
                .await
                .expect("acquire")
        );
        // Incumbent renews.
        assert!(
            repo.try_acquire_job_lease("trace_cleanup", "node-a", 60)
                .await
                .expect("renew")
        );
        // A different holder cannot take an unexpired lease.
        assert!(
            !repo
                .try_acquire_job_lease("trace_cleanup", "node-b", 60)
                .await
                .expect("contend")
        );
        // Different job is independent.
        assert!(
            repo.try_acquire_job_lease("dlq_retry", "node-b", 60)
                .await
                .expect("other job")
        );
    }

    #[tokio::test]
    async fn test_job_lease_expiry_allows_takeover() {
        let repo = test_repo().await;
        assert!(
            repo.try_acquire_job_lease("job", "node-a", 60)
                .await
                .expect("acquire")
        );
        // Force-expire the lease.
        let DbPool::Sqlite(p) = &repo.pool else {
            unreachable!("sqlite expected");
        };
        sqlx::query("UPDATE job_leases SET expires_at = datetime('now', '-10 seconds')")
            .execute(p)
            .await
            .expect("expire");
        assert!(
            repo.try_acquire_job_lease("job", "node-b", 60)
                .await
                .expect("takeover")
        );
    }
}
