use async_trait::async_trait;
use sea_query::{Asterisk, Expr, Order, Query};

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

            let (sql, values) = build_sqlx(&mut query);
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
