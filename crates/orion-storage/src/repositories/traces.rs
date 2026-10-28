use crate::DbPool;
use async_trait::async_trait;
use sea_query::{Asterisk, Condition, Expr, ExprTrait, IntoIden, Query};
use serde::Deserialize;

use super::helpers::{Page, PaginatedResult, Projection};
use crate::models::{self, Trace, TraceListRow};
use crate::{build_sqlx, schema::Traces};
use orion_common::errors::OrionError;

#[derive(Debug, Default, Deserialize, serde::Serialize, utoipa::IntoParams)]
#[into_params(parameter_in = Query)]
pub struct TraceFilter {
    /// Filter by trace status.
    pub status: Option<String>,
    /// Filter by channel.
    pub channel: Option<String>,
    /// Filter by mode: `sync`, `async` or `cron` — the transport the
    /// message arrived on.
    pub mode: Option<String>,
    /// Page size, clamped to [1, 1000] (default 50).
    pub limit: Option<i64>,
    /// Resume after this position: the previous page's `next_cursor`, unmodified.
    pub cursor: Option<String>,
    /// Compute `total` for this page (default false). Off by default:
    /// the count is a full scan of the filtered set on Postgres and InnoDB,
    /// paid on every page even though the number rarely changes what the
    /// caller does next.
    pub include_total: Option<bool>,
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
    /// A fatal execution failure (timeout or engine error).
    pub failed: bool,
    pub duration_ms: f64,
    /// Optional per-task execution trace JSON. Populated only when the
    /// channel has `config.tracing.task_details = true`.
    pub task_trace_json: Option<String>,
}

/// Borrowed view of a completed-trace row, so the singular path can hand its
/// `&str` arguments straight to the insert builder instead of copying the
/// request body and the engine result into a throwaway owned row.
///
/// It is also [`TraceSink::store_completed`]'s parameter: seven
/// positional arguments, four of them `Option`/`&str` and interchangeable by
/// type, is a signature where a transposition compiles.
pub struct TraceCompletedRef<'a> {
    pub channel: &'a str,
    /// The channel's stable ID (as opposed to `channel`, its name). `None`
    /// when the channel could not be resolved at write time.
    pub channel_id: Option<&'a str>,
    pub mode: &'a str,
    pub input_json: Option<&'a str>,
    pub result_json: &'a str,
    /// A fatal execution failure (timeout or engine error).
    pub failed: bool,
    pub duration_ms: f64,
    /// Optional per-task execution trace JSON. Populated only when the
    /// channel has `config.tracing.task_details = true`.
    pub task_trace_json: Option<&'a str>,
}

impl TraceCompletedRow {
    /// Borrow this row as a [`TraceCompletedRef`]. Deliberately not spelled
    /// `as_ref`: an inherent method by that name trips
    /// `clippy::should_implement_trait`, which CI runs as `-D warnings`.
    pub fn as_view(&self) -> TraceCompletedRef<'_> {
        TraceCompletedRef {
            channel: &self.channel,
            channel_id: self.channel_id.as_deref(),
            mode: &self.mode,
            input_json: self.input_json.as_deref(),
            result_json: &self.result_json,
            failed: self.failed,
            duration_ms: self.duration_ms,
            task_trace_json: self.task_trace_json.as_deref(),
        }
    }
}

/// Owned payload for a queued [`TraceSink::complete`]: a successful run's
/// result. The queue completes a trace only on success — a failure is a
/// status write — so the row carries no status of its own.
#[derive(Debug, Clone)]
pub struct TraceResultRow {
    pub id: String,
    pub result_json: String,
    pub duration_ms: f64,
    /// Optional per-task execution trace JSON. Populated only when the
    /// channel has `config.tracing.task_details = true`.
    pub task_trace_json: Option<String>,
}

impl TraceResultRow {
    /// This row as the completion it stands for: `completed`, no error.
    pub fn as_completion(&self) -> TraceCompletion<'_> {
        TraceCompletion {
            result_json: &self.result_json,
            duration_ms: self.duration_ms,
            task_trace_json: self.task_trace_json.as_deref(),
            status: models::TRACE_STATUS_COMPLETED,
            error_message: None,
        }
    }
}

/// A trace's last write — its result and its terminal status — which
/// [`TraceSink::complete`] makes in one UPDATE.
///
/// One, not two: split into the result and the status, the queued `batch`
/// mode could write the status at once and the result with the next flush,
/// so a poller would see `completed` with no result.
#[derive(Debug, Clone, Copy)]
pub struct TraceCompletion<'a> {
    pub result_json: &'a str,
    pub duration_ms: f64,
    /// Optional per-task execution trace JSON. Populated only when the
    /// channel has `config.tracing.task_details = true`.
    pub task_trace_json: Option<&'a str>,
    /// The terminal status: `completed` or `failed`.
    pub status: &'a str,
    /// Written only when present, as [`TraceSink::update_status`] does.
    pub error_message: Option<&'a str>,
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
    row: TraceCompletedRef<'_>,
    id: &str,
    now: chrono::NaiveDateTime,
) -> [sea_query::SimpleExpr; 11] {
    let input_val = super::helpers::optional_string_value(row.input_json);
    let task_trace_val = super::helpers::optional_string_value(row.task_trace_json);
    let channel_id_val = super::helpers::optional_string_value(row.channel_id);
    [
        Expr::val(id),
        Expr::val(if row.failed { "failed" } else { "completed" }),
        Expr::val(row.channel),
        Expr::val(channel_id_val),
        Expr::val(row.mode),
        Expr::val(input_val),
        Expr::val(row.result_json),
        Expr::val(row.duration_ms),
        Expr::val(now),
        Expr::val(now),
        Expr::val(task_trace_val),
    ]
}

/// The 11 columns a trace *listing* reads, in
/// [`crate::models::TraceListRow`] order.
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
fn list_page(filter: &TraceFilter) -> Result<Page, OrionError> {
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

    Ok(Page {
        from: Traces::Table.into_iden(),
        projection: Projection::Columns(
            list_columns()
                .into_iter()
                .map(IntoIden::into_iden)
                .collect(),
        ),
        cond,
        // Newest first, the id as the tie-break: served by
        // `idx_traces_created_at_id`.
        created_at: Traces::CreatedAt.into_iden(),
        id: Traces::Id.into_iden(),
        limit: super::helpers::clamp_limit(filter.limit),
        cursor: super::helpers::parse_cursor(filter.cursor.as_deref())?,
        // Opt-in: COUNT(*) over the filtered set is a full scan on Postgres and
        // InnoDB, and it was being paid on every page.
        include_total: filter.include_total.unwrap_or(false),
    })
}

/// `SELECT * FROM traces WHERE id = ?` — the single-row read shape, shared by
/// `get_by_id` and the read-back arm of the write-returning paths.
fn trace_select(id: &str) -> sea_query::SelectStatement {
    Query::select()
        .column(Asterisk)
        .from(Traces::Table)
        .and_where(Expr::col(Traces::Id).eq(id))
        .to_owned()
}

fn trace_not_found(id: &str) -> OrionError {
    OrionError::not_found(format!("Trace '{id}' not found"))
}

/// The UPDATE both completion paths share, for the same reason.
fn complete_update(
    id: &str,
    completion: TraceCompletion<'_>,
    now: chrono::NaiveDateTime,
) -> sea_query::UpdateStatement {
    let task_trace_val = super::helpers::optional_string_value(completion.task_trace_json);
    let mut update = Query::update()
        .table(Traces::Table)
        .value(Traces::UpdatedAt, super::helpers::updated_now())
        .value(Traces::Status, completion.status)
        .value(Traces::ResultJson, completion.result_json)
        .value(Traces::DurationMs, completion.duration_ms)
        .value(Traces::TaskTraceJson, task_trace_val)
        .value(Traces::CompletedAt, now)
        .to_owned();
    if let Some(err) = completion.error_message {
        update.value(Traces::ErrorMessage, err);
    }
    update.and_where(Expr::col(Traces::Id).eq(id)).to_owned()
}

// -- Repository trait --

/// Writing a trace: what the request path and the background workers do.
///
/// Split from reading and from retention because the three have no consumer in
/// common. The persistence queue needs these methods and nothing else, so a
/// test double for it does not have to stub out a paginated listing it never
/// touches.
///
/// The split is what makes an alternative sink writable at all — an OTel or
/// ClickHouse exporter can satisfy this and could never satisfy
/// [`TraceReader::list_paginated`].
#[async_trait]
pub trait TraceSink: Send + Sync {
    async fn create_pending(
        &self,
        channel: &str,
        channel_id: Option<&str>,
        mode: &str,
        input_json: Option<&str>,
        access_token_hash: Option<&str>,
    ) -> Result<Trace, OrionError>;
    /// Set a trace's status (and its start or completion time). `NotFound`
    /// when no trace has `id`. Answers nothing else: every caller only needs
    /// to know the write landed, and reading the row back cost a SELECT of
    /// its input, result and task trace.
    async fn update_status(
        &self,
        id: &str,
        status: &str,
        error_message: Option<&str>,
    ) -> Result<(), OrionError>;
    /// Write a trace's result and terminal status in one statement, setting
    /// `completed_at`. `NotFound` when no trace has `id`.
    async fn complete(&self, id: &str, completion: TraceCompletion<'_>) -> Result<(), OrionError>;
    async fn store_completed(&self, row: TraceCompletedRef<'_>) -> Result<String, OrionError>;

    /// Batched variant of [`Self::store_completed`]. Default impl loops the singular
    /// method; backend implementations can override with a single multi-row
    /// INSERT inside one transaction (huge win on WAL-mode SQLite where each
    /// individual commit is a fsync).
    async fn store_completed_batch(
        &self,
        rows: &[TraceCompletedRow],
    ) -> Result<Vec<String>, OrionError> {
        let mut ids = Vec::with_capacity(rows.len());
        for row in rows {
            ids.push(self.store_completed(row.as_view()).await?);
        }
        Ok(ids)
    }

    /// Batched variant of [`Self::complete`]. Default impl loops the singular
    /// method; backend implementations can override using one transaction.
    async fn complete_batch(&self, rows: &[TraceResultRow]) -> Result<(), OrionError> {
        for row in rows {
            self.complete(&row.id, row.as_completion()).await?;
        }
        Ok(())
    }
}

/// Reading traces back: the `/api/v2/data/traces` endpoints.
#[async_trait]
pub trait TraceReader: Send + Sync {
    async fn get_by_id(&self, id: &str) -> Result<Trace, OrionError>;
    /// One page of the trace listing, payload- and credential-free.
    ///
    /// Carries [`TraceListRow`], not [`Trace`]: a `SELECT *` would read every
    /// caller's request body, the full engine message and one
    /// `access_token_hash` per row out of the database for a response that
    /// shows none of them.
    async fn list_paginated(
        &self,
        filter: &TraceFilter,
    ) -> Result<PaginatedResult<TraceListRow>, OrionError>;
}

/// Ageing traces out: the retention job, and nothing else.
#[async_trait]
pub trait TraceRetention: Send + Sync {
    /// Delete traces older than the given number of hours. Returns the count deleted.
    async fn delete_older_than(&self, hours: u64) -> Result<u64, OrionError>;
}

/// Everything a full SQL-backed trace store does.
///
/// `AppState` holds this, and hands each consumer the narrower trait it
/// actually uses — trait upcasting (stable since 1.86, and the MSRV is 1.98)
/// makes `Arc<dyn TraceRepository>` coerce to any of the three. Nothing
/// implements this directly: the blanket impl means implementing the three
/// parts is what makes a type a repository.
pub trait TraceRepository: TraceSink + TraceReader + TraceRetention {}

impl<T: TraceSink + TraceReader + TraceRetention> TraceRepository for T {}

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
impl TraceSink for SqlTraceRepository {
    async fn create_pending(
        &self,
        channel: &str,
        channel_id: Option<&str>,
        mode: &str,
        input_json: Option<&str>,
        access_token_hash: Option<&str>,
    ) -> Result<Trace, OrionError> {
        orion_common::metrics::timed_db_op("traces.create_pending", async {
            let id = uuid::Uuid::now_v7().to_string();

            let input_val = super::helpers::optional_string_value(input_json);
            let channel_id_val = super::helpers::optional_string_value(channel_id);
            let token_hash_val = super::helpers::optional_string_value(access_token_hash);

            let mut insert = Query::insert();
            insert
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
                    Expr::val(id.as_str()),
                    Expr::val("pending"),
                    Expr::val(channel),
                    Expr::val(channel_id_val),
                    Expr::val(mode),
                    Expr::val(input_val),
                    Expr::val(token_hash_val),
                ]);

            // D23: the INSERT and the row it wrote travel together.
            super::helpers::write_returning_row(
                &self.pool,
                super::helpers::WriteStatement::Insert(&mut insert),
                &mut trace_select(&id),
                OrionError::Storage,
                || trace_not_found(&id),
            )
            .await
        })
        .await
    }

    async fn update_status(
        &self,
        id: &str,
        status: &str,
        error_message: Option<&str>,
    ) -> Result<(), OrionError> {
        orion_common::metrics::timed_db_op("traces.update_status", async {
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
            update
                .table(Traces::Table)
                .value(Traces::UpdatedAt, super::helpers::updated_now())
                .value(Traces::Status, status);

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

            let (sql, values) = build_sqlx(self.pool.backend(), &mut update);
            match self.pool.execute_query(&sql, values).await? {
                0 => Err(trace_not_found(id)),
                _ => Ok(()),
            }
        })
        .await
    }

    async fn complete(&self, id: &str, completion: TraceCompletion<'_>) -> Result<(), OrionError> {
        orion_common::metrics::timed_db_op("traces.complete", async {
            let now = chrono::Utc::now().naive_utc();
            let (sql, values) = build_sqlx(
                self.pool.backend(),
                &mut complete_update(id, completion, now),
            );
            match self.pool.execute_query(&sql, values).await? {
                0 => Err(trace_not_found(id)),
                _ => Ok(()),
            }
        })
        .await
    }

    async fn store_completed(&self, row: TraceCompletedRef<'_>) -> Result<String, OrionError> {
        orion_common::metrics::timed_db_op("traces.store_completed", async {
            let id = uuid::Uuid::now_v7().to_string();
            let now = chrono::Utc::now().naive_utc();

            let (sql, values) = build_sqlx(
                self.pool.backend(),
                Query::insert()
                    .into_table(Traces::Table)
                    .columns(completed_columns())
                    .values_panic(completed_values(row, &id, now)),
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
        orion_common::metrics::timed_db_op("traces.store_completed_batch", async {
            let now = chrono::Utc::now().naive_utc();
            let mut ids = Vec::with_capacity(rows.len());
            let mut insert = Query::insert();
            insert
                .into_table(Traces::Table)
                .columns(completed_columns());
            for row in rows {
                let id = uuid::Uuid::now_v7().to_string();
                insert.values_panic(completed_values(row.as_view(), &id, now));
                ids.push(id);
            }
            let (sql, values) = build_sqlx(self.pool.backend(), &mut insert);
            self.pool.execute_query(&sql, values).await?;
            orion_common::metrics::record_trace_persistence_batch_size(rows.len());
            Ok(ids)
        })
        .await
    }

    async fn complete_batch(&self, rows: &[TraceResultRow]) -> Result<(), OrionError> {
        if rows.is_empty() {
            return Ok(());
        }
        orion_common::metrics::timed_db_op("traces.complete_batch", async {
            let now = chrono::Utc::now().naive_utc();
            let mut tx = self.pool.begin_tx().await.map_err(OrionError::Storage)?;
            for row in rows {
                let (sql, values) = build_sqlx(
                    self.pool.backend(),
                    &mut complete_update(&row.id, row.as_completion(), now),
                );
                tx.execute_query(&sql, values).await?;
            }
            tx.commit().await.map_err(OrionError::Storage)?;
            orion_common::metrics::record_trace_persistence_batch_size(rows.len());
            Ok(())
        })
        .await
    }
}

#[async_trait]
impl TraceReader for SqlTraceRepository {
    async fn get_by_id(&self, id: &str) -> Result<Trace, OrionError> {
        orion_common::metrics::timed_db_op("traces.get_by_id", async {
            let (sql, values) = build_sqlx(self.pool.backend(), &mut trace_select(id));

            self.pool
                .fetch_optional_as::<Trace>(&sql, values)
                .await?
                .ok_or_else(|| trace_not_found(id))
        })
        .await
    }

    async fn list_paginated(
        &self,
        filter: &TraceFilter,
    ) -> Result<PaginatedResult<TraceListRow>, OrionError> {
        orion_common::metrics::timed_db_op("traces.list_paginated", async {
            // The page — projection (D27), filter and keyset position — comes
            // from `list_page`, the same builder the SQL-shape test asserts
            // against, so it cannot drift from the column list or reintroduce a
            // `SELECT *`.
            super::helpers::paginate(&self.pool, list_page(filter)?).await
        })
        .await
    }
}

#[async_trait]
impl TraceRetention for SqlTraceRepository {
    async fn delete_older_than(&self, hours: u64) -> Result<u64, OrionError> {
        orion_common::metrics::timed_db_op("traces.delete_older_than", async {
            let now = chrono::Utc::now().naive_utc();
            let cutoff = super::helpers::cutoff_hours_ago(now, hours);
            // D4: rows stuck in `pending`/`running` (queue submit failed,
            // worker died mid-flight) must match a retention predicate too,
            // or they leak forever — an unbounded leak on the hottest table,
            // hidden because retention *looks* configured.
            // Real processing is bounded by processing_timeout_ms (seconds),
            // so anything non-terminal at twice the retention window is
            // unambiguously dead.
            let stuck_cutoff = super::helpers::cutoff_hours_ago(now, hours.saturating_mul(2));

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

    async fn test_pool() -> crate::DbPool {
        crate::test_sqlite_pool().await
    }

    /// The listing must never read the payload columns or the capability
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
            .list_paginated(&TraceFilter {
                // `total` is opt-in as of D8.
                include_total: Some(true),
                ..Default::default()
            })
            .await
            .expect("test");
        assert_eq!(page.total, Some(1));
        assert_eq!(page.data[0].id, trace.id);
        assert_eq!(page.data[0].channel, "orders");

        // The exact statement `list_paginated` runs, not a re-typed copy.
        let sql =
            super::super::helpers::page_select(&list_page(&TraceFilter::default()).expect("test"))
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
            .store_completed(TraceCompletedRef {
                failed: false,
                channel: "orders",
                channel_id: Some("ch_orders"),
                mode: "sync",
                input_json: None,
                result_json: r#"{"ok":true}"#,
                duration_ms: 10.0,
                task_trace_json: None,
            })
            .await
            .expect("test");

        // Backdate it to 100 hours ago
        let old_time = chrono::Utc::now()
            .naive_utc()
            .checked_sub_signed(chrono::Duration::hours(100))
            .expect("test")
            .to_string();
        match &pool {
            crate::DbPool::Sqlite(p) => {
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
            .store_completed(TraceCompletedRef {
                failed: false,
                channel: "orders",
                channel_id: Some("ch_orders"),
                mode: "sync",
                input_json: None,
                result_json: r#"{"ok":true}"#,
                duration_ms: 5.0,
                task_trace_json: None,
            })
            .await
            .expect("test");

        // Delete traces older than 72 hours
        let deleted = repo.delete_older_than(72).await.expect("test");
        assert_eq!(deleted, 1);

        // Verify the recent trace still exists
        let remaining = repo
            .list_paginated(&TraceFilter {
                include_total: Some(true),
                ..Default::default()
            })
            .await
            .expect("test");
        assert_eq!(remaining.total, Some(1));
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
            crate::DbPool::Sqlite(p) => {
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
        // failed) are never completed. Beyond twice the retention window they
        // are unambiguously dead and must be reclaimed.
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
                crate::DbPool::Sqlite(p) => {
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

    // ------------------------------------------------------------------
    // D8: opt-in total, keyset pagination.
    // ------------------------------------------------------------------

    /// Seed `n` completed traces one second apart, oldest first, so the
    /// created_at ordering is unambiguous. Returns the ids in insert order.
    async fn seed_traces(pool: &crate::DbPool, n: usize) -> Vec<String> {
        let repo = SqlTraceRepository::new(pool.clone());
        let mut ids = Vec::new();
        let base = chrono::Utc::now().naive_utc();
        for i in 0..n {
            let id = repo
                .store_completed(TraceCompletedRef {
                    failed: false,
                    channel: "orders",
                    channel_id: Some("ch_orders"),
                    mode: "sync",
                    input_json: None,
                    result_json: "{}",
                    duration_ms: 1.0,
                    task_trace_json: None,
                })
                .await
                .expect("test");
            let stamp = base
                .checked_sub_signed(chrono::Duration::seconds((n - i) as i64))
                .expect("test")
                .to_string();
            match pool {
                crate::DbPool::Sqlite(p) => {
                    sqlx::query("UPDATE traces SET created_at = ? WHERE id = ?")
                        .bind(&stamp)
                        .bind(&id)
                        .execute(p)
                        .await
                        .expect("test");
                }
                _ => unreachable!("Test requires SQLite"),
            }
            ids.push(id);
        }
        ids
    }

    #[tokio::test]
    async fn test_total_is_opt_in() {
        let pool = test_pool().await;
        seed_traces(&pool, 3).await;
        let repo = SqlTraceRepository::new(pool.clone());

        let default_page = repo
            .list_paginated(&TraceFilter::default())
            .await
            .expect("test");
        assert_eq!(
            default_page.total, None,
            "the count scans the filtered set; it must not be paid unasked"
        );
        assert_eq!(default_page.data.len(), 3);

        let counted = repo
            .list_paginated(&TraceFilter {
                include_total: Some(true),
                ..Default::default()
            })
            .await
            .expect("test");
        assert_eq!(counted.total, Some(3));
    }

    #[tokio::test]
    async fn test_keyset_pagination_walks_every_row_exactly_once() {
        let pool = test_pool().await;
        let seeded = seed_traces(&pool, 7).await;
        let repo = SqlTraceRepository::new(pool.clone());

        let mut seen = Vec::new();
        let mut cursor = None;
        loop {
            let page = repo
                .list_paginated(&TraceFilter {
                    limit: Some(3),
                    cursor: cursor.clone(),
                    ..Default::default()
                })
                .await
                .expect("test");
            seen.extend(page.data.iter().map(|t| t.id.clone()));
            match page.next_cursor {
                Some(next) => cursor = Some(next),
                None => break,
            }
            assert!(seen.len() <= 7, "cursor walk is not terminating");
        }

        // Default order is created_at DESC — newest first, so the reverse of
        // insert order, with every row present exactly once.
        let mut expected = seeded;
        expected.reverse();
        assert_eq!(seen, expected);
    }

    /// The tie-break the cursor exists for: rows sharing a `created_at` must
    /// not be skipped or repeated at a page boundary.
    #[tokio::test]
    async fn test_keyset_pagination_is_stable_across_identical_timestamps() {
        let pool = test_pool().await;
        let repo = SqlTraceRepository::new(pool.clone());
        for _ in 0..5 {
            repo.store_completed(TraceCompletedRef {
                failed: false,
                channel: "orders",
                channel_id: Some("ch_orders"),
                mode: "sync",
                input_json: None,
                result_json: "{}",
                duration_ms: 1.0,
                task_trace_json: None,
            })
            .await
            .expect("test");
        }
        let same = "2026-01-01 00:00:00";
        match &pool {
            crate::DbPool::Sqlite(p) => {
                sqlx::query("UPDATE traces SET created_at = ?")
                    .bind(same)
                    .execute(p)
                    .await
                    .expect("test");
            }
            _ => unreachable!("Test requires SQLite"),
        }

        let mut seen = std::collections::BTreeSet::new();
        let mut total = 0usize;
        let mut cursor = None;
        loop {
            let page = repo
                .list_paginated(&TraceFilter {
                    limit: Some(2),
                    cursor: cursor.clone(),
                    ..Default::default()
                })
                .await
                .expect("test");
            total += page.data.len();
            for row in &page.data {
                seen.insert(row.id.clone());
            }
            match page.next_cursor {
                Some(next) => cursor = Some(next),
                None => break,
            }
            assert!(total <= 5, "cursor walk is not terminating");
        }
        assert_eq!(seen.len(), 5, "every row must appear");
        assert_eq!(total, 5, "and none of them twice");
    }

    #[tokio::test]
    async fn test_a_malformed_cursor_is_refused() {
        let pool = test_pool().await;
        let repo = SqlTraceRepository::new(pool.clone());
        let malformed = repo
            .list_paginated(&TraceFilter {
                cursor: Some("not-a-cursor".to_string()),
                ..Default::default()
            })
            .await;
        assert!(matches!(malformed, Err(OrionError::Validation { .. })));
    }

    /// The list is ordered by `(created_at, id)`, so it must be backed by an
    /// index. Asserted against the schema rather than a plan, so it holds on
    /// every backend.
    #[tokio::test]
    async fn test_the_list_order_is_indexed() {
        let pool = test_pool().await;
        let crate::DbPool::Sqlite(p) = &pool else {
            unreachable!("Test requires SQLite");
        };
        let indexes: Vec<(String,)> = sqlx::query_as(
            "SELECT name FROM sqlite_master WHERE type = 'index' AND tbl_name = 'traces'",
        )
        .fetch_all(p)
        .await
        .expect("test");
        let names: Vec<String> = indexes.into_iter().map(|(n,)| n).collect();
        let expected = "idx_traces_created_at_id";
        assert!(
            names.iter().any(|n| n == expected),
            "{expected} must exist — the list is ordered by it, and an unindexed \
             order full-scans the traces table. Have: {names:?}"
        );
    }
}
