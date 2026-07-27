use async_trait::async_trait;
use sea_query::{Asterisk, Expr, Order, Query, SimpleExpr};

use crate::errors::OrionError;
use crate::storage::models::AuditLogEntry;
use crate::storage::schema::AuditLogs;
use crate::storage::{DbPool, build_sqlx};

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

    /// List audit log entries with pagination, newest first.
    async fn list_paginated(
        &self,
        offset: i64,
        limit: i64,
    ) -> Result<Vec<AuditLogEntry>, OrionError>;

    /// Count total audit log entries.
    async fn count(&self) -> Result<i64, OrionError>;
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
        offset: i64,
        limit: i64,
    ) -> Result<Vec<AuditLogEntry>, OrionError> {
        crate::metrics::timed_db_op("audit_logs.list", async {
            let (sql, values) = build_sqlx(
                Query::select()
                    .column(Asterisk)
                    .from(AuditLogs::Table)
                    .order_by(AuditLogs::CreatedAt, Order::Desc)
                    .offset(offset as u64)
                    .limit(limit as u64),
            );

            self.pool
                .fetch_all_as::<AuditLogEntry>(&sql, values)
                .await
                .map_err(|e| OrionError::InternalSource {
                    context: "Failed to list audit logs".to_string(),
                    source: Box::new(e),
                })
        })
        .await
    }

    async fn count(&self) -> Result<i64, OrionError> {
        crate::metrics::timed_db_op("audit_logs.count", async {
            let (sql, values) = build_sqlx(
                Query::select()
                    .expr(Expr::col(Asterisk).count())
                    .from(AuditLogs::Table),
            );

            let row: (i64,) = self
                .pool
                .fetch_one_as::<(i64,)>(&sql, values)
                .await
                .map_err(|e| OrionError::InternalSource {
                    context: "Failed to count audit logs".to_string(),
                    source: Box::new(e),
                })?;
            Ok(row.0)
        })
        .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn test_repo() -> SqlAuditLogRepository {
        SqlAuditLogRepository::new(crate::storage::test_sqlite_pool().await)
    }

    #[tokio::test]
    async fn test_insert_without_details_persists() {
        let repo = test_repo().await;
        repo.insert("admin...", "create", "workflow", "wf-1", None)
            .await
            .expect("insert without details");

        let entries = repo.list_paginated(0, 10).await.expect("list");
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].action, "create");
        assert!(entries[0].details.is_none());
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

        let entries = repo.list_paginated(0, 10).await.expect("list");
        assert_eq!(entries.len(), 1, "exactly one row, not two");
        let entry = &entries[0];
        assert_eq!(entry.principal, "admin...");
        assert_eq!(entry.action, "activate");
        assert_eq!(entry.resource_type, "workflow");
        assert_eq!(entry.resource_id, "wf-1");
        assert_eq!(entry.details.as_deref(), Some(details));
        assert_eq!(repo.count().await.expect("count"), 1);
    }
}
