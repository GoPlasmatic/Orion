//! Cluster coordination repository: config epoch, breaker-reset fan-out,
//! and job leases (multi-instance-ha A2/A7/A11).
//!
//! All statements use each backend's own clock (`datetime('now')` /
//! `LOCALTIMESTAMP` / `UTC_TIMESTAMP()`) so lease comparisons never depend
//! on node clocks — only the DB's. Those clock expressions are the only raw
//! SQL here (`Expr::cust`); everything else is sea-query, so placeholder
//! dialects are the builder's problem, not ours.

use async_trait::async_trait;
use sea_query::{Expr, ExprTrait, Query, SimpleExpr};

use crate::errors::OrionError;
use crate::storage::schema::{ConfigEpoch, JobLeases};
use crate::storage::{DbBackend, DbPool, build_sqlx};

use super::helpers::{fetch_required, insert_if_absent, update_returning_scalar};

/// The single `config_epoch` row.
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct EpochRow {
    pub epoch: i64,
    pub breaker_epoch: i64,
    pub breaker_key: String,
    /// What the bumping node changed, as
    /// [`crate::cluster::EpochScope::as_str`] wrote it. Empty for a row an
    /// older writer bumped, which a reader treats as "everything" — see
    /// [`crate::cluster::EpochScope::parse`].
    pub epoch_scope: String,
    /// The epoch [`Self::epoch_scope`] was written for. Zero on a row last
    /// bumped by a writer that predates the column — and, more importantly,
    /// stale whenever such a writer bumped *after* a scope-aware one, which
    /// is the case [`crate::cluster::EpochScope::for_epoch`] exists to catch.
    pub epoch_scope_at: i64,
}

#[async_trait]
pub trait ClusterRepository: Send + Sync {
    /// Atomically increment the config epoch, recording what changed;
    /// returns the post-increment value.
    async fn bump_epoch(&self, scope: &str) -> Result<i64, OrionError>;

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

    /// The DB server's own clock as a SQL expression.
    fn now_expr(backend: DbBackend) -> SimpleExpr {
        Expr::cust(super::helpers::sql_now(backend))
    }

    fn missing_epoch_row() -> OrionError {
        OrionError::internal(
            "config_epoch row missing — cluster_coordination migration not applied".to_string(),
        )
    }

    /// `UPDATE config_epoch ... WHERE id = 1` returning `returning_col`, with
    /// the MySQL read-back handled by [`update_returning_scalar`]. Shared by
    /// `bump_epoch` and `request_breaker_reset`.
    async fn update_epoch_row(
        &self,
        mut update: sea_query::UpdateStatement,
        returning_col: ConfigEpoch,
    ) -> Result<i64, OrionError> {
        update
            .value(ConfigEpoch::UpdatedAt, Self::now_expr(self.pool.backend()))
            .and_where(Expr::col(ConfigEpoch::Id).eq(1));
        let mut read_back = Query::select()
            .column(returning_col)
            .from(ConfigEpoch::Table)
            .and_where(Expr::col(ConfigEpoch::Id).eq(1))
            .to_owned();
        update_returning_scalar(
            &self.pool,
            &mut update,
            returning_col,
            &mut read_back,
            Self::missing_epoch_row,
        )
        .await
    }
}

#[async_trait]
impl ClusterRepository for SqlClusterRepository {
    async fn bump_epoch(&self, scope: &str) -> Result<i64, OrionError> {
        crate::metrics::timed_db_op("cluster.bump_epoch", async {
            // Counter and scope in one statement: a reader that sees the new
            // epoch always sees the scope that goes with it.
            let update = Query::update()
                .table(ConfigEpoch::Table)
                // `epoch_scope_at` is assigned BEFORE `epoch`, and the order
                // is load-bearing: MySQL evaluates a multi-column SET left to
                // right and lets a later assignment read a column the same
                // statement already updated, while SQLite and PostgreSQL
                // evaluate every right-hand side against the old row. Written
                // first, `epoch + 1` is the same new value on all three.
                .value(
                    ConfigEpoch::EpochScopeAt,
                    Expr::col(ConfigEpoch::Epoch).add(1),
                )
                .value(ConfigEpoch::Epoch, Expr::col(ConfigEpoch::Epoch).add(1))
                .value(ConfigEpoch::EpochScope, scope)
                .to_owned();
            self.update_epoch_row(update, ConfigEpoch::Epoch).await
        })
        .await
    }

    async fn get_epoch(&self) -> Result<EpochRow, OrionError> {
        crate::metrics::timed_db_op("cluster.get_epoch", async {
            let (sql, values) = build_sqlx(
                self.pool.backend(),
                Query::select()
                    .columns([
                        ConfigEpoch::Epoch,
                        ConfigEpoch::BreakerEpoch,
                        ConfigEpoch::BreakerKey,
                        ConfigEpoch::EpochScope,
                        ConfigEpoch::EpochScopeAt,
                    ])
                    .from(ConfigEpoch::Table)
                    .and_where(Expr::col(ConfigEpoch::Id).eq(1)),
            );
            fetch_required(&self.pool, &sql, values, Self::missing_epoch_row).await
        })
        .await
    }

    async fn request_breaker_reset(&self, key: &str) -> Result<i64, OrionError> {
        crate::metrics::timed_db_op("cluster.request_breaker_reset", async {
            let update = Query::update()
                .table(ConfigEpoch::Table)
                .value(
                    ConfigEpoch::BreakerEpoch,
                    Expr::col(ConfigEpoch::BreakerEpoch).add(1),
                )
                .value(ConfigEpoch::BreakerKey, key)
                .to_owned();
            self.update_epoch_row(update, ConfigEpoch::BreakerEpoch)
                .await
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
            let expiry: SimpleExpr = Expr::cust(super::helpers::sql_now_plus_secs(
                self.pool.backend(),
                ttl_secs,
            ));

            // Step 1: renew own lease or take over an expired one.
            let (sql, values) = build_sqlx(
                self.pool.backend(),
                Query::update()
                    .table(JobLeases::Table)
                    .value(JobLeases::Holder, holder)
                    .value(JobLeases::ExpiresAt, expiry.clone())
                    .and_where(Expr::col(JobLeases::JobName).eq(job_name))
                    .and_where(Expr::col(JobLeases::Holder).eq(holder).or(
                        Expr::col(JobLeases::ExpiresAt).lt(Self::now_expr(self.pool.backend())),
                    )),
            );
            if self.pool.execute_query(&sql, values).await? > 0 {
                return Ok(true);
            }

            // Step 2: no row (first acquisition) — insert-if-absent. A loss
            // to a concurrent inserter means another node holds the lease.
            let insert = Query::insert()
                .into_table(JobLeases::Table)
                .columns([JobLeases::JobName, JobLeases::Holder, JobLeases::ExpiresAt])
                .values_panic([job_name.into(), holder.into(), expiry])
                .to_owned();
            Ok(insert_if_absent(&self.pool, insert, JobLeases::JobName).await? > 0)
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
        assert_eq!(repo.bump_epoch("definitions").await.expect("bump"), 1);
        assert_eq!(repo.bump_epoch("definitions").await.expect("bump"), 2);
        assert_eq!(repo.get_epoch().await.expect("get").epoch, 2);
    }

    /// A bump stamps the scope with the epoch it produced, so a reader can
    /// tell this change's scope from one left behind by an earlier bump.
    /// Both columns move in one statement — the read-back must never show a
    /// scope trailing its epoch by one, which is what a right-hand side
    /// evaluated against the *new* `epoch` would produce.
    #[tokio::test]
    async fn a_bump_binds_the_scope_to_the_epoch_it_wrote() {
        let repo = test_repo().await;
        for expected in 1..=3 {
            assert_eq!(repo.bump_epoch("connectors").await.expect("bump"), expected);
            let row = repo.get_epoch().await.expect("get");
            assert_eq!(row.epoch, expected);
            assert_eq!(row.epoch_scope_at, expected);
            assert_eq!(row.epoch_scope, "connectors");
            assert_eq!(
                crate::cluster::EpochScope::for_epoch(
                    row.epoch,
                    row.epoch_scope_at,
                    &row.epoch_scope
                ),
                crate::cluster::EpochScope::Connectors
            );
        }
    }

    /// The rolling-deploy case, reproduced against the real column: a writer
    /// that predates the binding advances `epoch` alone. The scope it leaves
    /// behind is recognised, so only `epoch_scope_at` can disqualify it.
    #[tokio::test]
    async fn an_unstamped_bump_leaves_the_previous_scope_untrusted() {
        let repo = test_repo().await;
        repo.bump_epoch("definitions").await.expect("bump");

        // What a 1.3.x binary's bump_epoch does: the counter, nothing else.
        let update = Query::update()
            .table(ConfigEpoch::Table)
            .value(ConfigEpoch::Epoch, Expr::col(ConfigEpoch::Epoch).add(1))
            .to_owned();
        repo.update_epoch_row(update, ConfigEpoch::Epoch)
            .await
            .expect("legacy bump");

        let row = repo.get_epoch().await.expect("get");
        assert_eq!(row.epoch, 2);
        assert_eq!(row.epoch_scope, "definitions", "the column is sticky");
        assert_eq!(row.epoch_scope_at, 1, "and it still names the old epoch");
        assert_eq!(
            crate::cluster::EpochScope::for_epoch(row.epoch, row.epoch_scope_at, &row.epoch_scope),
            crate::cluster::EpochScope::All,
            "an unattributable scope must cost the wide resync"
        );
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

    /// The Postgres/MySQL arms cannot execute without containers (CI covers
    /// that); at least pin the rendered SQL shapes so a sea-query upgrade or
    /// helper change that breaks them fails here first.
    #[test]
    fn per_backend_sql_shapes() {
        use sea_query::{MysqlQueryBuilder, PostgresQueryBuilder};

        // Postgres: UPDATE ... RETURNING with builder-managed placeholders.
        let mut update = Query::update()
            .table(ConfigEpoch::Table)
            .value(ConfigEpoch::BreakerKey, "k")
            .and_where(Expr::col(ConfigEpoch::Id).eq(1))
            .to_owned();
        update.returning(Query::returning().column(ConfigEpoch::BreakerEpoch));
        let (sql, _) = update.build(PostgresQueryBuilder);
        assert!(sql.contains("RETURNING \"breaker_epoch\""), "{sql}");
        assert!(sql.contains("$1"), "{sql}");

        // MySQL: insert_if_absent patches the verb on the rendered SQL.
        let insert = Query::insert()
            .into_table(JobLeases::Table)
            .columns([JobLeases::JobName, JobLeases::Holder])
            .values_panic(["j".into(), "h".into()])
            .to_owned();
        let (sql, _) = insert.build(MysqlQueryBuilder);
        let patched = sql.replacen("INSERT INTO", "INSERT IGNORE INTO", 1);
        assert!(patched.starts_with("INSERT IGNORE INTO"), "{patched}");

        // Postgres: insert-if-absent renders ON CONFLICT DO NOTHING.
        let mut insert = Query::insert()
            .into_table(JobLeases::Table)
            .columns([JobLeases::JobName, JobLeases::Holder])
            .values_panic(["j".into(), "h".into()])
            .to_owned();
        insert.on_conflict(
            sea_query::OnConflict::column(JobLeases::JobName)
                .do_nothing()
                .to_owned(),
        );
        let (sql, _) = insert.build(PostgresQueryBuilder);
        assert!(
            sql.contains("ON CONFLICT (\"job_name\") DO NOTHING"),
            "{sql}"
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
