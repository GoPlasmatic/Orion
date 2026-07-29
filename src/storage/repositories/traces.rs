use crate::storage::DbPool;
use async_trait::async_trait;
use sea_query::{Asterisk, Condition, Expr, IntoIden, Query};
use serde::Deserialize;

use super::helpers::{Page, PaginatedResult, Projection};
use crate::errors::OrionError;
use crate::storage::models::{self, Trace, TraceListRow};
use crate::storage::{build_sqlx, schema::Traces};

#[derive(Debug, Default, Deserialize)]
pub struct TraceFilter {
    pub status: Option<String>,
    pub channel: Option<String>,
    pub mode: Option<String>,
    pub limit: Option<i64>,
    pub offset: Option<i64>,
    /// Column to sort by: created_at (default), updated_at, status, channel, mode.
    pub sort_by: Option<String>,
    /// Sort direction: asc or desc (default).
    pub sort_order: Option<String>,
}

/// Owned payload for a batched `store_completed` write.
#[derive(Debug, Clone)]
pub struct TraceCompletedRow {
    pub channel: String,
    /// The channel's stable ID (as opposed to `channel`, its name).
    /// `None` when the channel could not be resolved at write time.
    pub channel_id: Option<String>,
    pub mode: String,
    pub input_json: Option<String>,
    pub result_json: String,
    pub duration_ms: f64,
    /// Optional per-task execution trace JSON. Populated only when the
    /// channel has `config.tracing.task_details = true` (A2).
    pub task_trace_json: Option<String>,
}

/// Owned payload for a batched `set_result` write.
#[derive(Debug, Clone)]
pub struct TraceResultRow {
    pub id: String,
    pub result_json: String,
    pub duration_ms: f64,
    /// Optional per-task execution trace JSON. Populated only when the
    /// channel has `config.tracing.task_details = true` (A2).
    pub task_trace_json: Option<String>,
}

/// The 11 columns every completed-trace INSERT writes. The singular and batch
/// paths share this (and [`completed_values`]) so they cannot diverge.
fn completed_columns() -> [Traces; 11] {
    [
        Traces::Id,
        Traces::Status,
        Traces::Channel,
        Traces::ChannelId,
        Traces::Mode,
        Traces::InputJson,
        Traces::ResultJson,
        Traces::DurationMs,
        Traces::StartedAt,
        Traces::CompletedAt,
        Traces::TaskTraceJson,
    ]
}

/// One completed-trace row's values, in [`completed_columns`] order.
fn completed_values(
    row: &TraceCompletedRow,
    id: &str,
    now: chrono::NaiveDateTime,
) -> [sea_query::SimpleExpr; 11] {
    let input_val = super::helpers::optional_string_value(row.input_json.as_deref());
    let task_trace_val = super::helpers::optional_string_value(row.task_trace_json.as_deref());
    let channel_id_val = super::helpers::optional_string_value(row.channel_id.as_deref());
    [
        Expr::val(id).into(),
        Expr::val("completed").into(),
        Expr::val(row.channel.as_str()).into(),
        Expr::val(channel_id_val).into(),
        Expr::val(row.mode.as_str()).into(),
        Expr::val(input_val).into(),
        Expr::val(row.result_json.as_str()).into(),
        Expr::val(row.duration_ms).into(),
        Expr::val(now).into(),
        Expr::val(now).into(),
        Expr::val(task_trace_val).into(),
    ]
}

/// The 11 columns a trace *listing* reads, in
/// [`crate::storage::models::TraceListRow`] order (D27).
///
/// Named explicitly so the four columns that are not here — `input_json`,
/// `result_json`, `task_trace_json` and `access_token_hash` — cannot come back
/// by way of someone reaching for `SELECT *`.
fn list_columns() -> [Traces; 11] {
    [
        Traces::Id,
        Traces::Channel,
        Traces::ChannelId,
        Traces::Mode,
        Traces::Status,
        Traces::ErrorMessage,
        Traces::DurationMs,
        Traces::StartedAt,
        Traces::CompletedAt,
        Traces::CreatedAt,
        Traces::UpdatedAt,
    ]
}

/// The page `list_paginated` reads: filter, sort and the [`list_columns`]
/// projection. A function, not an inline block, so a test can assert the
/// exact SQL the repository runs rather than a re-typed copy of it.
fn list_page(filter: &TraceFilter) -> Page {
    let (limit, offset) = super::helpers::clamp_pagination(filter.limit, filter.offset);

    let mut cond = Condition::all();
    if let Some(ref status) = filter.status {
        cond = cond.add(Expr::col(Traces::Status).eq(status.as_str()));
    }
    if let Some(ref channel) = filter.channel {
        cond = cond.add(Expr::col(Traces::Channel).eq(channel.as_str()));
    }
    if let Some(ref mode) = filter.mode {
        cond = cond.add(Expr::col(Traces::Mode).eq(mode.as_str()));
    }

    let sort = match filter.sort_by.as_deref() {
        Some("updated_at") => Traces::UpdatedAt,
        Some("status") => Traces::Status,
        Some("channel") => Traces::Channel,
        Some("mode") => Traces::Mode,
        _ => Traces::CreatedAt,
    };

    Page {
        from: Traces::Table.into_iden(),
        projection: Projection::Columns(
            list_columns()
                .into_iter()
                .map(IntoIden::into_iden)
                .collect(),
        ),
        cond,
        sort: sort.into_iden(),
        order: super::helpers::parse_sort_order(filter.sort_order.as_deref()),
        limit,
        offset,
    }
}

/// The UPDATE both result-write paths share, for the same reason.
fn result_update(row: &TraceResultRow) -> sea_query::UpdateStatement {
    let task_trace_val = super::helpers::optional_string_value(row.task_trace_json.as_deref());
    Query::update()
        .table(Traces::Table)
        .value(Traces::ResultJson, row.result_json.as_str())
        .value(Traces::DurationMs, row.duration_ms)
        .value(Traces::TaskTraceJson, task_trace_val)
        .and_where(Expr::col(Traces::Id).eq(row.id.as_str()))
        .to_owned()
}

// -- Repository trait --

#[async_trait]
pub trait TraceRepository: Send + Sync {
    async fn create_pending(
        &self,
        channel: &str,
        channel_id: Option<&str>,
        mode: &str,
        input_json: Option<&str>,
        access_token_hash: Option<&str>,
    ) -> Result<Trace, OrionError>;
    async fn get_by_id(&self, id: &str) -> Result<Trace, OrionError>;
    async fn update_status(
        &self,
        id: &str,
        status: &str,
        error_message: Option<&str>,
    ) -> Result<Trace, OrionError>;
    async fn set_result(
        &self,
        id: &str,
        result_json: &str,
        duration_ms: f64,
        task_trace_json: Option<&str>,
    ) -> Result<(), OrionError>;
    #[allow(clippy::too_many_arguments)]
    async fn store_completed(
        &self,
        channel: &str,
        channel_id: Option<&str>,
        mode: &str,
        input_json: Option<&str>,
        result_json: &str,
        duration_ms: f64,
        task_trace_json: Option<&str>,
    ) -> Result<String, OrionError>;

    /// Batched variant of [`store_completed`]. Default impl loops the singular
    /// method; backend implementations can override with a single multi-row
    /// INSERT inside one transaction (huge win on WAL-mode SQLite where each
    /// individual commit is a fsync).
    async fn store_completed_batch(
        &self,
        rows: &[TraceCompletedRow],
    ) -> Result<Vec<String>, OrionError> {
        let mut ids = Vec::with_capacity(rows.len());
        for row in rows {
            ids.push(
                self.store_completed(
                    &row.channel,
                    row.channel_id.as_deref(),
                    &row.mode,
                    row.input_json.as_deref(),
                    &row.result_json,
                    row.duration_ms,
                    row.task_trace_json.as_deref(),
                )
                .await?,
            );
        }
        Ok(ids)
    }

    /// Batched variant of [`set_result`]. Default impl loops the singular
    /// method; backend implementations can override using one transaction.
    async fn set_result_batch(&self, rows: &[TraceResultRow]) -> Result<(), OrionError> {
        for row in rows {
            self.set_result(
                &row.id,
                &row.result_json,
                row.duration_ms,
                row.task_trace_json.as_deref(),
            )
            .await?;
        }
        Ok(())
    }

    /// One page of the trace listing, payload- and credential-free.
    ///
    /// Returns [`TraceListRow`], not [`Trace`] (D27): the list used to be a
    /// `SELECT *`, which read every caller's request body, the full engine
    /// message and one `access_token_hash` per row out of the database for a
    /// response that shows none of them.
    async fn list_paginated(
        &self,
        filter: &TraceFilter,
    ) -> Result<PaginatedResult<TraceListRow>, OrionError>;
    /// Delete traces older than the given number of hours. Returns the count deleted.
    async fn delete_older_than(&self, hours: u64) -> Result<u64, OrionError>;
}

// -- SQL implementation --

pub struct SqlTraceRepository {
    pool: DbPool,
}

impl SqlTraceRepository {
    pub fn new(pool: DbPool) -> Self {
        Self { pool }
    }
}

#[async_trait]
impl TraceRepository for SqlTraceRepository {
    async fn create_pending(
        &self,
        channel: &str,
        channel_id: Option<&str>,
        mode: &str,
        input_json: Option<&str>,
        access_token_hash: Option<&str>,
    ) -> Result<Trace, OrionError> {
        crate::metrics::timed_db_op("traces.create_pending", async {
            let id = uuid::Uuid::new_v4().to_string();

            let input_val = super::helpers::optional_string_value(input_json);
            let channel_id_val = super::helpers::optional_string_value(channel_id);
            let token_hash_val = super::helpers::optional_string_value(access_token_hash);

            let (sql, values) = build_sqlx(
                Query::insert()
                    .into_table(Traces::Table)
                    .columns([
                        Traces::Id,
                        Traces::Status,
                        Traces::Channel,
                        Traces::ChannelId,
                        Traces::Mode,
                        Traces::InputJson,
                        Traces::AccessTokenHash,
                    ])
                    .values_panic([
                        Expr::val(id.as_str()).into(),
                        Expr::val("pending").into(),
                        Expr::val(channel).into(),
                        Expr::val(channel_id_val).into(),
                        Expr::val(mode).into(),
                        Expr::val(input_val).into(),
                        Expr::val(token_hash_val).into(),
                    ]),
            );

            self.pool.execute_query(&sql, values).await?;

            self.get_by_id(&id).await
        })
        .await
    }

    async fn get_by_id(&self, id: &str) -> Result<Trace, OrionError> {
        crate::metrics::timed_db_op("traces.get_by_id", async {
            let (sql, values) = build_sqlx(
                Query::select()
                    .column(Asterisk)
                    .from(Traces::Table)
                    .and_where(Expr::col(Traces::Id).eq(id)),
            );

            self.pool
                .fetch_optional_as::<Trace>(&sql, values)
                .await?
                .ok_or_else(|| OrionError::NotFound(format!("Trace '{id}' not found")))
        })
        .await
    }

    async fn update_status(
        &self,
        id: &str,
        status: &str,
        error_message: Option<&str>,
    ) -> Result<Trace, OrionError> {
        crate::metrics::timed_db_op("traces.update_status", async {
            // Bind chrono values, not strings: postgres rejects TEXT
            // parameters against timestamp columns (42804).
            let now = chrono::Utc::now().naive_utc();

            let (started_at, completed_at) = if status == models::TRACE_STATUS_RUNNING {
                (Some(now), None)
            } else if status == models::TRACE_STATUS_COMPLETED
                || status == models::TRACE_STATUS_FAILED
            {
                (None, Some(now))
            } else {
                (None, None)
            };

            let mut update = Query::update();
            update.table(Traces::Table).value(Traces::Status, status);

            if let Some(err) = error_message {
                update.value(Traces::ErrorMessage, err);
            }
            if let Some(sa) = started_at {
                update.value(Traces::StartedAt, sa);
            }
            if let Some(ca) = completed_at {
                update.value(Traces::CompletedAt, ca);
            }

            update.and_where(Expr::col(Traces::Id).eq(id));

            let (sql, values) = build_sqlx(&mut update);

            self.pool.execute_query(&sql, values).await?;

            self.get_by_id(id).await
        })
        .await
    }

    async fn set_result(
        &self,
        id: &str,
        result_json: &str,
        duration_ms: f64,
        task_trace_json: Option<&str>,
    ) -> Result<(), OrionError> {
        crate::metrics::timed_db_op("traces.set_result", async {
            let row = TraceResultRow {
                id: id.to_string(),
                result_json: result_json.to_string(),
                duration_ms,
                task_trace_json: task_trace_json.map(str::to_string),
            };
            let (sql, values) = build_sqlx(&mut result_update(&row));
            self.pool.execute_query(&sql, values).await?;
            Ok(())
        })
        .await
    }

    async fn store_completed(
        &self,
        channel: &str,
        channel_id: Option<&str>,
        mode: &str,
        input_json: Option<&str>,
        result_json: &str,
        duration_ms: f64,
        task_trace_json: Option<&str>,
    ) -> Result<String, OrionError> {
        crate::metrics::timed_db_op("traces.store_completed", async {
            let id = uuid::Uuid::new_v4().to_string();
            let now = chrono::Utc::now().naive_utc();
            let row = TraceCompletedRow {
                channel: channel.to_string(),
                channel_id: channel_id.map(str::to_string),
                mode: mode.to_string(),
                input_json: input_json.map(str::to_string),
                result_json: result_json.to_string(),
                duration_ms,
                task_trace_json: task_trace_json.map(str::to_string),
            };

            let (sql, values) = build_sqlx(
                Query::insert()
                    .into_table(Traces::Table)
                    .columns(completed_columns())
                    .values_panic(completed_values(&row, &id, now)),
            );

            self.pool.execute_query(&sql, values).await?;

            Ok(id)
        })
        .await
    }

    async fn store_completed_batch(
        &self,
        rows: &[TraceCompletedRow],
    ) -> Result<Vec<String>, OrionError> {
        if rows.is_empty() {
            return Ok(Vec::new());
        }
        crate::metrics::timed_db_op("traces.store_completed_batch", async {
            let now = chrono::Utc::now().naive_utc();
            let mut ids = Vec::with_capacity(rows.len());
            let mut insert = Query::insert();
            insert
                .into_table(Traces::Table)
                .columns(completed_columns());
            for row in rows {
                let id = uuid::Uuid::new_v4().to_string();
                insert.values_panic(completed_values(row, &id, now));
                ids.push(id);
            }
            let (sql, values) = build_sqlx(&mut insert);
            self.pool.execute_query(&sql, values).await?;
            crate::metrics::record_trace_persistence_batch_size(rows.len());
            Ok(ids)
        })
        .await
    }

    async fn set_result_batch(&self, rows: &[TraceResultRow]) -> Result<(), OrionError> {
        if rows.is_empty() {
            return Ok(());
        }
        crate::metrics::timed_db_op("traces.set_result_batch", async {
            let mut tx = self.pool.begin_tx().await.map_err(OrionError::Storage)?;
            for row in rows {
                let (sql, values) = build_sqlx(&mut result_update(row));
                tx.execute_query(&sql, values).await?;
            }
            tx.commit().await.map_err(OrionError::Storage)?;
            crate::metrics::record_trace_persistence_batch_size(rows.len());
            Ok(())
        })
        .await
    }

    async fn list_paginated(
        &self,
        filter: &TraceFilter,
    ) -> Result<PaginatedResult<TraceListRow>, OrionError> {
        crate::metrics::timed_db_op("traces.list_paginated", async {
            super::helpers::paginate(&self.pool, list_page(filter)).await
        })
        .await
    }

    async fn delete_older_than(&self, hours: u64) -> Result<u64, OrionError> {
        crate::metrics::timed_db_op("traces.delete_older_than", async {
            let now = chrono::Utc::now().naive_utc();
            let cutoff = now
                .checked_sub_signed(chrono::Duration::hours(hours as i64))
                .unwrap_or(chrono::NaiveDateTime::MIN);
            // D4: rows stuck in `pending`/`running` (queue submit failed,
            // worker died mid-flight) previously matched no retention
            // predicate and leaked forever — an unbounded leak on the
            // hottest table, hidden because retention *looked* configured.
            // Real processing is bounded by processing_timeout_ms (seconds),
            // so anything non-terminal at twice the retention window is
            // unambiguously dead.
            let stuck_cutoff = now
                .checked_sub_signed(chrono::Duration::hours((hours as i64).saturating_mul(2)))
                .unwrap_or(chrono::NaiveDateTime::MIN);

            // D6: chunked, not one unbounded statement — the first tick after
            // retention is enabled can span millions of rows.
            super::helpers::delete_chunked(
                &self.pool,
                Traces::Table,
                Traces::Id,
                Condition::any()
                    .add(
                        Expr::col(Traces::CreatedAt)
                            .lt(cutoff)
                            .and(Expr::col(Traces::Status).is_in(["completed", "failed"])),
                    )
                    .add(Expr::col(Traces::CreatedAt).lt(stuck_cutoff)),
            )
            .await
        })
        .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn test_pool() -> crate::storage::DbPool {
        crate::storage::test_sqlite_pool().await
    }

    /// D27: the listing must never read the payload columns or the capability
    /// -token hash. `TraceListRow` cannot hold them, but sqlx ignores extra
    /// columns — so a revert to `SELECT *` would still decode, and would still
    /// pull one credential verifier per row across the wire from the database.
    /// The projection itself is therefore what is asserted.
    #[tokio::test]
    async fn test_list_paginated_reads_a_narrow_projection() {
        let pool = test_pool().await;
        let repo = SqlTraceRepository::new(pool.clone());

        let trace = repo
            .create_pending(
                "orders",
                Some("ch_orders"),
                "async",
                Some(r#"{"card":"4111111111111111"}"#),
                Some("sha256-of-the-capability-token"),
            )
            .await
            .expect("test");

        let page = repo
            .list_paginated(&TraceFilter::default())
            .await
            .expect("test");
        assert_eq!(page.total, 1);
        assert_eq!(page.data[0].id, trace.id);
        assert_eq!(page.data[0].channel, "orders");

        // The exact statement `list_paginated` runs, not a re-typed copy.
        let sql = super::super::helpers::page_select(&list_page(&TraceFilter::default()))
            .to_string(sea_query::SqliteQueryBuilder);
        for withheld in [
            "input_json",
            "result_json",
            "task_trace_json",
            "access_token_hash",
        ] {
            assert!(
                !sql.contains(withheld),
                "the trace listing projection names `{withheld}`: {sql}"
            );
        }
        assert!(
            !sql.contains('*'),
            "the trace listing must name its columns, not `SELECT *`: {sql}"
        );
    }

    #[tokio::test]
    async fn test_delete_older_than_removes_old_completed_traces() {
        let pool = test_pool().await;
        let repo = SqlTraceRepository::new(pool.clone());

        // Create a completed trace
        let id = repo
            .store_completed(
                "orders",
                Some("ch_orders"),
                "sync",
                None,
                r#"{"ok":true}"#,
                10.0,
                None,
            )
            .await
            .expect("test");

        // Backdate it to 100 hours ago
        let old_time = chrono::Utc::now()
            .naive_utc()
            .checked_sub_signed(chrono::Duration::hours(100))
            .expect("test")
            .to_string();
        match &pool {
            crate::storage::DbPool::Sqlite(p) => {
                sqlx::query("UPDATE traces SET created_at = ? WHERE id = ?")
                    .bind(&old_time)
                    .bind(&id)
                    .execute(p)
                    .await
                    .expect("test");
            }
            _ => unreachable!("Test requires SQLite"),
        }

        // Create a recent trace that should NOT be deleted
        let _recent_id = repo
            .store_completed(
                "orders",
                Some("ch_orders"),
                "sync",
                None,
                r#"{"ok":true}"#,
                5.0,
                None,
            )
            .await
            .expect("test");

        // Delete traces older than 72 hours
        let deleted = repo.delete_older_than(72).await.expect("test");
        assert_eq!(deleted, 1);

        // Verify the recent trace still exists
        let remaining = repo
            .list_paginated(&TraceFilter::default())
            .await
            .expect("test");
        assert_eq!(remaining.total, 1);
    }

    #[tokio::test]
    async fn test_delete_older_than_preserves_recent_pending_traces() {
        let pool = test_pool().await;
        let repo = SqlTraceRepository::new(pool.clone());

        // Create a pending trace
        let trace = repo
            .create_pending("orders", Some("ch_orders"), "async", None, None)
            .await
            .expect("test");

        // Backdate it past the retention window but inside the 2× stuck
        // window: old enough that a *terminal* row would be deleted, not yet
        // old enough to be declared stuck.
        let old_time = chrono::Utc::now()
            .naive_utc()
            .checked_sub_signed(chrono::Duration::hours(100))
            .expect("test")
            .to_string();
        match &pool {
            crate::storage::DbPool::Sqlite(p) => {
                sqlx::query("UPDATE traces SET created_at = ? WHERE id = ?")
                    .bind(&old_time)
                    .bind(&trace.id)
                    .execute(p)
                    .await
                    .expect("test");
            }
            _ => unreachable!("Test requires SQLite"),
        }

        let deleted = repo.delete_older_than(72).await.expect("test");
        assert_eq!(deleted, 0);
    }

    #[tokio::test]
    async fn test_delete_older_than_reclaims_stuck_traces() {
        // D4: pending/running rows whose worker died (or whose queue submit
        // failed) previously leaked forever. Beyond twice the retention
        // window they are unambiguously dead and must be reclaimed.
        let pool = test_pool().await;
        let repo = SqlTraceRepository::new(pool.clone());

        for status in ["pending", "running"] {
            let trace = repo
                .create_pending("orders", Some("ch_orders"), "async", None, None)
                .await
                .expect("test");
            let old_time = chrono::Utc::now()
                .naive_utc()
                .checked_sub_signed(chrono::Duration::hours(200))
                .expect("test")
                .to_string();
            match &pool {
                crate::storage::DbPool::Sqlite(p) => {
                    sqlx::query("UPDATE traces SET created_at = ?, status = ? WHERE id = ?")
                        .bind(&old_time)
                        .bind(status)
                        .bind(&trace.id)
                        .execute(p)
                        .await
                        .expect("test");
                }
                _ => unreachable!("Test requires SQLite"),
            }
        }

        // 200h > 2 × 72h → both stuck rows deleted.
        let deleted = repo.delete_older_than(72).await.expect("test");
        assert_eq!(deleted, 2);
    }
}
