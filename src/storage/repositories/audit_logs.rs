use async_trait::async_trait;
use chrono::NaiveDateTime;
use sea_query::{Condition, Expr, IntoIden, Order, Query, SimpleExpr};

use super::helpers::{Page, PaginatedResult, Projection};
use crate::errors::OrionError;
use crate::storage::models::AuditLogEntry;
use crate::storage::schema::AuditLogs;
use crate::storage::{DbPool, build_sqlx};

/// Server-side filter for audit-log reads (O8). Every field is an exact match
/// except the half-open time window `[start_time, end_time)`; unset fields do
/// not constrain the query.
///
/// `deny_unknown_fields` is deliberate: before O8 the handler accepted only
/// `offset`/`limit`, so `?action=activate` was dropped and the caller got a
/// 200 with unfiltered data. A rejected typo is better than a silently wrong
/// compliance answer.
#[derive(Debug, Clone, Default, serde::Deserialize, utoipa::IntoParams)]
#[serde(deny_unknown_fields)]
#[into_params(parameter_in = Query)]
pub struct AuditLogFilter {
    /// Exact-match filter on the action (e.g. `create`, `activate`, `delete`).
    pub action: Option<String>,
    /// Exact-match filter on the resource type (`workflow`, `channel`,
    /// `connector`, `engine`).
    pub resource_type: Option<String>,
    /// Exact-match filter on the resource ID.
    pub resource_id: Option<String>,
    /// Exact-match filter on the acting principal.
    pub principal: Option<String>,
    /// Inclusive lower bound on `created_at`, RFC 3339
    /// (e.g. `2026-07-01T00:00:00Z`) or a bare naive timestamp.
    #[serde(default, deserialize_with = "de_timestamp")]
    #[param(value_type = Option<String>)]
    pub start_time: Option<NaiveDateTime>,
    /// Exclusive upper bound on `created_at`, RFC 3339 or a bare naive
    /// timestamp.
    #[serde(default, deserialize_with = "de_timestamp")]
    #[param(value_type = Option<String>)]
    pub end_time: Option<NaiveDateTime>,
    /// Page size, clamped to [1, 1000] (default 50).
    pub limit: Option<i64>,
    /// Pagination offset (default 0).
    pub offset: Option<i64>,
}

/// Accepts RFC 3339 (`2026-07-01T00:00:00Z`) or a bare naive timestamp
/// (`2026-07-01T00:00:00` / `2026-07-01 00:00:00`); `created_at` is stored
/// timezone-naive in UTC, so offsets are normalized to UTC first.
pub(crate) fn parse_timestamp(raw: &str) -> Result<NaiveDateTime, String> {
    if let Ok(dt) = chrono::DateTime::parse_from_rfc3339(raw) {
        return Ok(dt.naive_utc());
    }
    NaiveDateTime::parse_from_str(raw, "%Y-%m-%dT%H:%M:%S")
        .or_else(|_| NaiveDateTime::parse_from_str(raw, "%Y-%m-%d %H:%M:%S"))
        .map_err(|_| format!("expected an RFC 3339 timestamp, got '{raw}'"))
}

/// [`parse_timestamp`] as a serde field deserializer, so the filter can be
/// extracted straight from the query string (D22 — the route used to carry a
/// parallel string-typed DTO purely to host this parse).
fn de_timestamp<'de, D>(deserializer: D) -> Result<Option<NaiveDateTime>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let raw: Option<String> = serde::Deserialize::deserialize(deserializer)?;
    raw.as_deref()
        .map(|r| parse_timestamp(r).map_err(serde::de::Error::custom))
        .transpose()
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

            // D18: one shared count-then-page. A driver error on the data read
            // used to be wrapped as `InternalSource` here while the count half
            // of the same method returned `Storage`; both are now `Storage`,
            // like every other list path.
            super::helpers::paginate(
                &self.pool,
                Page {
                    from: AuditLogs::Table.into_iden(),
                    projection: Projection::All,
                    cond,
                    sort: AuditLogs::CreatedAt.into_iden(),
                    order: Order::Desc,
                    limit,
                    offset,
                },
            )
            .await
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

    #[test]
    fn parses_rfc3339_and_naive_timestamps() {
        let expected = chrono::NaiveDate::from_ymd_opt(2026, 7, 1)
            .expect("date")
            .and_hms_opt(12, 30, 0)
            .expect("time");
        assert_eq!(
            parse_timestamp("2026-07-01T12:30:00Z").expect("rfc3339"),
            expected
        );
        assert_eq!(
            parse_timestamp("2026-07-01T12:30:00").expect("naive"),
            expected
        );
        // Offsets are normalized to UTC before comparison with `created_at`.
        assert_eq!(
            parse_timestamp("2026-07-01T14:30:00+02:00").expect("offset"),
            expected
        );
    }

    #[test]
    fn rejects_malformed_timestamps() {
        assert!(parse_timestamp("yesterday").is_err());
    }

    /// The filter deserializes directly at the extraction boundary,
    /// timestamps and `deny_unknown_fields` included (D22 — the route used to
    /// carry a parallel string-typed DTO for this).
    #[test]
    fn filter_deserializes_with_timestamps_and_rejects_unknown_keys() {
        use serde_json::json;
        let f: AuditLogFilter = serde_json::from_value(
            json!({"action": "activate", "start_time": "2026-07-01T00:00:00Z"}),
        )
        .expect("deserialize");
        assert_eq!(f.action.as_deref(), Some("activate"));
        assert!(f.start_time.is_some());

        assert!(
            serde_json::from_value::<AuditLogFilter>(json!({"actoin": "activate"})).is_err(),
            "a misspelled key must be refused, not silently dropped (O8)"
        );
        assert!(
            serde_json::from_value::<AuditLogFilter>(json!({"start_time": "yesterday"})).is_err()
        );
    }

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
