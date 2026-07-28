use async_trait::async_trait;
use chrono::NaiveDateTime;
use sea_query::{Asterisk, Condition, Expr, Order, Query, SimpleExpr};

use super::helpers::PaginatedResult;
use crate::errors::OrionError;
use crate::storage::models::AuditLogEntry;
use crate::storage::schema::AuditLogs;
use crate::storage::{DbPool, build_sqlx};

/// Server-side filter for audit-log reads (O8). Every field is an exact match
/// except the half-open time window `[start_time, end_time)`; unset fields do
/// not constrain the query.
#[derive(Debug, Clone, Default)]
pub struct AuditLogFilter {
    pub action: Option<String>,
    pub resource_type: Option<String>,
    pub resource_id: Option<String>,
    pub principal: Option<String>,
    pub start_time: Option<NaiveDateTime>,
    pub end_time: Option<NaiveDateTime>,
    pub limit: Option<i64>,
    pub offset: Option<i64>,
}

#[async_trait]
pub trait AuditLogRepository: Send + Sync {
    /// Insert an audit log entry.
    async fn insert(
        &self,
        principal: &str,
        action: &str,
        resource_type: &str,
        resource_id: &str,
        details: Option<&str>,
    ) -> Result<(), OrionError>;

    /// List audit log entries matching `filter`, newest first. `total` counts
    /// the rows matching the filter, not the whole table.
    async fn list_paginated(
        &self,
        filter: &AuditLogFilter,
    ) -> Result<PaginatedResult<AuditLogEntry>, OrionError>;

    /// Delete audit log entries older than the given number of days.
    /// Returns the count deleted.
    async fn delete_older_than(&self, days: u64) -> Result<u64, OrionError>;
}

pub struct SqlAuditLogRepository {
    pool: DbPool,
}

impl SqlAuditLogRepository {
    pub fn new(pool: DbPool) -> Self {
        Self { pool }
    }
}

#[async_trait]
impl AuditLogRepository for SqlAuditLogRepository {
    async fn insert(
        &self,
        principal: &str,
        action: &str,
        resource_type: &str,
        resource_id: &str,
        details: Option<&str>,
    ) -> Result<(), OrionError> {
        crate::metrics::timed_db_op("audit_logs.insert", async {
            let id = uuid::Uuid::new_v4().to_string();

            // `columns()` REPLACES the column list and `values_panic()` appends
            // a whole new row — so the optional `details` column has to be folded
            // into a single columns/values pair, not added in a second call.
            let mut columns = vec![
                AuditLogs::Id,
                AuditLogs::Principal,
                AuditLogs::Action,
                AuditLogs::ResourceType,
                AuditLogs::ResourceId,
            ];
            let mut row: Vec<SimpleExpr> = vec![
                Expr::val(id.as_str()).into(),
                Expr::val(principal).into(),
                Expr::val(action).into(),
                Expr::val(resource_type).into(),
                Expr::val(resource_id).into(),
            ];
            if let Some(d) = details {
                columns.push(AuditLogs::Details);
                row.push(Expr::val(d).into());
            }

            let (sql, values) = build_sqlx(
                Query::insert()
                    .into_table(AuditLogs::Table)
                    .columns(columns)
                    .values_panic(row),
            );
            self.pool.execute_query(&sql, values).await?;
            Ok(())
        })
        .await
    }

    async fn list_paginated(
        &self,
        filter: &AuditLogFilter,
    ) -> Result<PaginatedResult<AuditLogEntry>, OrionError> {
        crate::metrics::timed_db_op("audit_logs.list", async {
            let (limit, offset) = super::helpers::clamp_pagination(filter.limit, filter.offset);

            // Every predicate is a bound parameter — no identifier or value is
            // ever interpolated into the SQL text.
            let mut cond = Condition::all();
            if let Some(ref action) = filter.action {
                cond = cond.add(Expr::col(AuditLogs::Action).eq(action.as_str()));
            }
            if let Some(ref resource_type) = filter.resource_type {
                cond = cond.add(Expr::col(AuditLogs::ResourceType).eq(resource_type.as_str()));
            }
            if let Some(ref resource_id) = filter.resource_id {
                cond = cond.add(Expr::col(AuditLogs::ResourceId).eq(resource_id.as_str()));
            }
            if let Some(ref principal) = filter.principal {
                cond = cond.add(Expr::col(AuditLogs::Principal).eq(principal.as_str()));
            }
            if let Some(start) = filter.start_time {
                cond = cond.add(Expr::col(AuditLogs::CreatedAt).gte(start));
            }
            if let Some(end) = filter.end_time {
                cond = cond.add(Expr::col(AuditLogs::CreatedAt).lt(end));
            }

            let total =
                super::helpers::count_where(&self.pool, AuditLogs::Table, cond.clone()).await?;

            let (sql, values) = build_sqlx(
                Query::select()
                    .column(Asterisk)
                    .from(AuditLogs::Table)
                    .cond_where(cond)
                    .order_by(AuditLogs::CreatedAt, Order::Desc)
                    .offset(offset as u64)
                    .limit(limit as u64),
            );

            let data = self
                .pool
                .fetch_all_as::<AuditLogEntry>(&sql, values)
                .await
                .map_err(|e| OrionError::InternalSource {
                    context: "Failed to list audit logs".to_string(),
                    source: Box::new(e),
                })?;

            Ok(PaginatedResult {
                data,
                total,
                limit,
                offset,
            })
        })
        .await
    }

    async fn delete_older_than(&self, days: u64) -> Result<u64, OrionError> {
        crate::metrics::timed_db_op("audit_logs.delete_older_than", async {
            let cutoff = chrono::Duration::try_days(days as i64)
                .and_then(|d| chrono::Utc::now().naive_utc().checked_sub_signed(d))
                .unwrap_or(chrono::NaiveDateTime::MIN);

            // D6: chunked — see `delete_chunked`.
            super::helpers::delete_chunked(
                &self.pool,
                AuditLogs::Table,
                AuditLogs::Id,
                sea_query::Condition::all().add(Expr::col(AuditLogs::CreatedAt).lt(cutoff)),
            )
            .await
        })
        .await
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    async fn test_repo() -> SqlAuditLogRepository {
        SqlAuditLogRepository::new(crate::storage::test_sqlite_pool().await)
    }

    async fn list_all(repo: &SqlAuditLogRepository) -> PaginatedResult<AuditLogEntry> {
        repo.list_paginated(&AuditLogFilter::default())
            .await
            .expect("list")
    }

    /// Backdate every row for `resource_id`, since `created_at` is set by a
    /// column default and cannot be supplied through `insert`.
    pub(crate) async fn backdate(pool: &DbPool, resource_id: &str, days: i64) {
        let when = chrono::Utc::now().naive_utc() - chrono::Duration::days(days);
        let (sql, values) = build_sqlx(
            Query::update()
                .table(AuditLogs::Table)
                .value(AuditLogs::CreatedAt, when)
                .and_where(Expr::col(AuditLogs::ResourceId).eq(resource_id)),
        );
        pool.execute_query(&sql, values).await.expect("backdate");
    }

    #[tokio::test]
    async fn test_insert_without_details_persists() {
        let repo = test_repo().await;
        repo.insert("admin...", "create", "workflow", "wf-1", None)
            .await
            .expect("insert without details");

        let page = list_all(&repo).await;
        assert_eq!(page.data.len(), 1);
        assert_eq!(page.data[0].action, "create");
        assert!(page.data[0].details.is_none());
    }

    /// D3: `columns()` replaces rather than appends, so the old two-call form
    /// produced `INSERT INTO audit_logs ("details") VALUES (5 values), (1 value)`.
    #[tokio::test]
    async fn test_insert_with_details_persists_and_reads_back() {
        let repo = test_repo().await;
        let details = r#"{"request_id":"req-123"}"#;
        repo.insert("admin...", "activate", "workflow", "wf-1", Some(details))
            .await
            .expect("insert with details must build valid SQL");

        let page = list_all(&repo).await;
        assert_eq!(page.data.len(), 1, "exactly one row, not two");
        assert_eq!(page.total, 1);
        let entry = &page.data[0];
        assert_eq!(entry.principal, "admin...");
        assert_eq!(entry.action, "activate");
        assert_eq!(entry.resource_type, "workflow");
        assert_eq!(entry.resource_id, "wf-1");
        assert_eq!(entry.details.as_deref(), Some(details));
    }

    /// D2: nothing removed audit rows before this, so the table grew forever.
    #[tokio::test]
    async fn test_delete_older_than_removes_only_expired_rows() {
        let pool = crate::storage::test_sqlite_pool().await;
        let repo = SqlAuditLogRepository::new(pool.clone());

        repo.insert("admin...", "create", "workflow", "old", None)
            .await
            .expect("insert old");
        repo.insert("admin...", "create", "workflow", "recent", None)
            .await
            .expect("insert recent");
        backdate(&pool, "old", 120).await;
        backdate(&pool, "recent", 10).await;

        let deleted = repo.delete_older_than(90).await.expect("cleanup");
        assert_eq!(deleted, 1, "only the 120-day-old row is past retention");

        let page = list_all(&repo).await;
        assert_eq!(page.data.len(), 1);
        assert_eq!(page.data[0].resource_id, "recent");
    }

    #[tokio::test]
    async fn test_delete_older_than_keeps_everything_when_nothing_expired() {
        let pool = crate::storage::test_sqlite_pool().await;
        let repo = SqlAuditLogRepository::new(pool.clone());

        repo.insert("admin...", "create", "workflow", "wf-1", None)
            .await
            .expect("insert");

        assert_eq!(repo.delete_older_than(1).await.expect("cleanup"), 0);
        assert_eq!(list_all(&repo).await.total, 1);
    }

    // -- O8: filtering --

    async fn seeded_repo() -> SqlAuditLogRepository {
        let repo = test_repo().await;
        for (principal, action, resource_type, resource_id) in [
            ("alice...", "create", "workflow", "wf-1"),
            ("alice...", "activate", "workflow", "wf-1"),
            ("bob...", "activate", "workflow", "wf-2"),
            ("bob...", "activate", "channel", "ch-1"),
            ("bob...", "delete", "connector", "conn-1"),
        ] {
            repo.insert(principal, action, resource_type, resource_id, None)
                .await
                .expect("seed");
        }
        repo
    }

    async fn matching(repo: &SqlAuditLogRepository, filter: AuditLogFilter) -> Vec<AuditLogEntry> {
        let page = repo.list_paginated(&filter).await.expect("list");
        assert_eq!(
            page.total,
            page.data.len() as i64,
            "total must count filtered rows, not the whole table"
        );
        page.data
    }

    #[tokio::test]
    async fn test_each_filter_narrows_results() {
        let repo = seeded_repo().await;
        assert_eq!(list_all(&repo).await.total, 5);

        assert_eq!(
            matching(
                &repo,
                AuditLogFilter {
                    action: Some("activate".into()),
                    ..Default::default()
                }
            )
            .await
            .len(),
            3
        );
        assert_eq!(
            matching(
                &repo,
                AuditLogFilter {
                    resource_type: Some("workflow".into()),
                    ..Default::default()
                }
            )
            .await
            .len(),
            3
        );
        assert_eq!(
            matching(
                &repo,
                AuditLogFilter {
                    resource_id: Some("wf-1".into()),
                    ..Default::default()
                }
            )
            .await
            .len(),
            2
        );
        assert_eq!(
            matching(
                &repo,
                AuditLogFilter {
                    principal: Some("bob...".into()),
                    ..Default::default()
                }
            )
            .await
            .len(),
            3
        );
    }

    #[tokio::test]
    async fn test_combined_filters_and_together() {
        let repo = seeded_repo().await;

        let rows = matching(
            &repo,
            AuditLogFilter {
                action: Some("activate".into()),
                resource_type: Some("workflow".into()),
                ..Default::default()
            },
        )
        .await;
        assert_eq!(rows.len(), 2);
        assert!(
            rows.iter()
                .all(|e| e.action == "activate" && e.resource_type == "workflow")
        );

        let rows = matching(
            &repo,
            AuditLogFilter {
                action: Some("activate".into()),
                resource_type: Some("workflow".into()),
                principal: Some("alice...".into()),
                ..Default::default()
            },
        )
        .await;
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].resource_id, "wf-1");

        // A conjunction that matches nothing must return nothing, not everything.
        assert!(
            matching(
                &repo,
                AuditLogFilter {
                    action: Some("create".into()),
                    resource_type: Some("connector".into()),
                    ..Default::default()
                }
            )
            .await
            .is_empty()
        );
    }

    #[tokio::test]
    async fn test_time_range_filter() {
        let pool = crate::storage::test_sqlite_pool().await;
        let repo = SqlAuditLogRepository::new(pool.clone());
        repo.insert("alice...", "create", "workflow", "old", None)
            .await
            .expect("seed old");
        repo.insert("alice...", "create", "workflow", "new", None)
            .await
            .expect("seed new");
        backdate(&pool, "old", 30).await;

        let cutoff = chrono::Utc::now().naive_utc() - chrono::Duration::days(7);

        let rows = matching(
            &repo,
            AuditLogFilter {
                start_time: Some(cutoff),
                ..Default::default()
            },
        )
        .await;
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].resource_id, "new");

        let rows = matching(
            &repo,
            AuditLogFilter {
                end_time: Some(cutoff),
                ..Default::default()
            },
        )
        .await;
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].resource_id, "old");
    }
}
