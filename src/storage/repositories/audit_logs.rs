use async_trait::async_trait;
use sea_query::{Asterisk, Expr, Order, Query};
use sea_query_binder::SqlxBinder;

use crate::errors::OrionError;
use crate::storage::models::AuditLogEntry;
use crate::storage::schema::AuditLogs;
use crate::storage::{DbPool, query_builder};

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

            let mut query = Query::insert()
                .into_table(AuditLogs::Table)
                .columns([
                    AuditLogs::Id,
                    AuditLogs::Principal,
                    AuditLogs::Action,
                    AuditLogs::ResourceType,
                    AuditLogs::ResourceId,
                ])
                .values_panic([
                    Expr::val(id.as_str()).into(),
                    Expr::val(principal).into(),
                    Expr::val(action).into(),
                    Expr::val(resource_type).into(),
                    Expr::val(resource_id).into(),
                ])
                .to_owned();

            if let Some(d) = details {
                query
                    .columns([AuditLogs::Details])
                    .values_panic([Expr::val(d).into()]);
            }

            let (sql, values) = query.build_sqlx(query_builder());
            sqlx::query_with(&sql, values)
                .execute(&self.pool)
                .await?;
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
            let (sql, values) = Query::select()
                .column(Asterisk)
                .from(AuditLogs::Table)
                .order_by(AuditLogs::CreatedAt, Order::Desc)
                .offset(offset as u64)
                .limit(limit as u64)
                .build_sqlx(query_builder());

            sqlx::query_as_with::<_, AuditLogEntry, _>(&sql, values)
                .fetch_all(&self.pool)
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
            let (sql, values) = Query::select()
                .expr(Expr::col(Asterisk).count())
                .from(AuditLogs::Table)
                .build_sqlx(query_builder());

            let row: (i64,) = sqlx::query_as_with(&sql, values)
                .fetch_one(&self.pool)
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
