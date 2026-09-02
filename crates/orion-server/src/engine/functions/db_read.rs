use std::sync::Arc;

use async_trait::async_trait;
use dataflow_rs::engine::task_context::TaskContext;
use serde_json::Value;

use super::connector_handler::{ConnectorHandler, Produced};
use super::connector_helpers::{
    ConnectorCall, decode_failure, reject_mongo_connector, require_op_allowed, resolve_bind_params,
    resolve_numeric_as, timed_query, to_connect_error,
};
use super::schema::{FieldKind, FieldSchema};
use super::templated_input::TemplatedInput;
use crate::connector::ConnectorRegistry;
use crate::connector::pool_cache::SqlPoolCache;
use crate::engine::HandlerError;

/// This handler's name, for the row-conversion helpers below — a reference to
/// the one place it is written (F48), not a second spelling of it.
const NAME: &str = <DbReadHandler as ConnectorHandler>::NAME;

/// Executes SQL SELECT queries against external databases configured via connectors.
pub struct DbReadHandler {
    pub pool_cache: Arc<SqlPoolCache>,
    pub registry: Arc<ConnectorRegistry>,
    /// Hard row cap, from `query.max_limit` (F10). Raw SQL can't have a
    /// LIMIT injected reliably, so rows are streamed and counted — one
    /// `SELECT * FROM big_table` must not OOM the process.
    pub max_rows: usize,
}

/// The statement and its bind values.
///
/// The SQL text is a literal read from the task; only the parameters come from
/// the message, which is what keeps them the sole request-controlled part of
/// the statement.
pub struct DbRead {
    query: String,
    params: Vec<Value>,
    numeric_as: crate::connector::sql_decode::NumericAs,
}

impl DbRead {
    /// The parse both raw-SQL handlers do. Shared rather than copied because
    /// `db_read` and `db_write` differ in what the database does with the
    /// statement, not in what the task says.
    pub(super) fn parse_statement(
        call: &ConnectorCall<'_>,
        input: &TemplatedInput,
        ctx: &TaskContext<'_>,
    ) -> Result<Self, HandlerError> {
        Ok(Self {
            query: call.require_str(input, "query")?.to_string(),
            params: resolve_bind_params(input, call.name, ctx)?,
            numeric_as: resolve_numeric_as(input, call.name, ctx)?,
        })
    }

    pub(super) fn query(&self) -> &str {
        &self.query
    }

    pub(super) fn params(&self) -> &[Value] {
        &self.params
    }

    pub(super) fn numeric_as(&self) -> crate::connector::sql_decode::NumericAs {
        self.numeric_as
    }
}

#[async_trait]
impl ConnectorHandler for DbReadHandler {
    const NAME: &'static str = "db_read";
    type Kind = crate::connector::kind::Db;
    type Input = TemplatedInput;
    type Parsed = DbRead;

    fn registry(&self) -> &Arc<ConnectorRegistry> {
        &self.registry
    }

    fn parse(
        &self,
        call: &ConnectorCall<'_>,
        input: &TemplatedInput,
        ctx: &TaskContext<'_>,
    ) -> Result<Self::Parsed, HandlerError> {
        DbRead::parse_statement(call, input, ctx)
    }

    fn gate(
        _parsed: &Self::Parsed,
        conn: &crate::connector::DbConnectorConfig,
        connector: &str,
    ) -> Result<(), HandlerError> {
        require_op_allowed(&conn.operations, "read", connector)?;
        // A Mongo connection string in a `db` connector is the right type and
        // the wrong backend: both are `ConnectorConfig::Db`, and only the
        // string tells them apart.
        Ok(reject_mongo_connector(
            <Self as ConnectorHandler>::NAME,
            connector,
            conn,
        )?)
    }

    async fn run(
        &self,
        read: Self::Parsed,
        db_config: &crate::connector::DbConnectorConfig,
        call: &ConnectorCall<'_>,
        _input: &TemplatedInput,
        _ctx: &mut TaskContext<'_>,
    ) -> Result<Produced, HandlerError> {
        let pool = self
            .pool_cache
            .get_pool(call.connector, db_config)
            .await
            .map_err(to_connect_error)?;

        let max_rows = self.max_rows;
        let params = read.params();
        let numeric = read.numeric_as();
        let query = read.query();

        // One body, three drivers: the macro binds the concrete pool, its
        // decoder and its binder, and each arm is type-checked on its own.
        let json = crate::connector::pool_cache::dispatch_sql_pool!(
            &pool, p, rows_to_json, bind => {
                let rows = timed_query(db_config.query_timeout_ms, call.name, async {
                    use futures::TryStreamExt;
                    let sqlx_query = bind(sqlx::query(query), params);
                    let mut stream = sqlx_query.fetch(p);
                    let mut rows = Vec::new();
                    // Not `.map_err(|e| e.to_string())`: stringifying here
                    // converted through `From<String>`, which is
                    // unconditionally a backend failure, so a constraint the
                    // driver had already classified was thrown away before
                    // `QueryFailure` could see it.
                    while let Some(row) = stream.try_next().await? {
                        if rows.len() >= max_rows {
                            // F42: classified so `timed_query` reports it as a
                            // 400 with the text intact rather than a 500 with
                            // the guidance sanitised away. The guidance *is*
                            // the message, so losing it loses the point.
                            return Err(
                                crate::engine::functions::connector_helpers::QueryFailure::Limit(
                                    format!(
                                        "{} result exceeds query.max_limit ({max_rows} rows) \
                                         — add a LIMIT to the query or raise the cap",
                                        call.name
                                    ),
                                ),
                            );
                        }
                        rows.push(row);
                    }
                    Ok(rows)
                })
                .await?;
                rows_to_json(&rows, numeric).map_err(|e| decode_failure(NAME, e))?
            }
        );

        Ok(Value::Array(json).into())
    }
}

// -- Input schema (F53) --
//
// The table describing this handler's `function.input` lives next to the
// handler it describes. It used to sit in `schema.rs` with the other nine,
// which is how every schema/handler divergence in the 1.0 audit happened:
// a field was added, renamed or made conditional here and the table saying
// so was in a different file.

pub(super) const DB_READ_FIELDS: &[FieldSchema] = &[
    FieldSchema {
        name: "connector",
        description: "Name of the SQL connector to query.",
        kind: FieldKind::String,
        required: true,
        ..FieldSchema::DEFAULT
    },
    FieldSchema {
        name: "query",
        description: "SQL query. Use $1, $2, ... placeholders bound from `params`.",
        kind: FieldKind::String,
        required: true,
        ..FieldSchema::DEFAULT
    },
    FieldSchema {
        name: "params",
        description: "Array of values to bind to query placeholders, in order. Accepts {\"var\": \"path\"} to read the value from the message.",
        kind: FieldKind::Array,
        resolvable: true,
        ..FieldSchema::DEFAULT
    },
    FieldSchema {
        name: "numeric_as",
        description: "How an arbitrary-precision decimal column is rendered: \"number\" (default) or \"string\". A number is computable in JSONLogic and rounds beyond 2^53 or on most decimal fractions; a string keeps every digit, which is what a money column needs.",
        kind: FieldKind::String,
        resolvable: true,
        ..FieldSchema::DEFAULT
    },
    FieldSchema {
        name: "output",
        description: "Dotted path in the message where rows are written. Defaults to \"data\".",
        kind: FieldKind::String,
        template_at: &[""],
        ..FieldSchema::DEFAULT
    },
];
