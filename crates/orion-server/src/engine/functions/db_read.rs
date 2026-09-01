use std::sync::Arc;

use async_trait::async_trait;
use dataflow_rs::engine::error::DataflowError;
use dataflow_rs::engine::task_context::TaskContext;
use serde_json::Value;
use sqlx::any::{AnyRow, AnyTypeInfoKind};
use sqlx::{Column, Row, ValueRef};

use super::connector_handler::{ConnectorHandler, Produced};
use super::connector_helpers::{
    ConnectorCall, bind_json_params, reject_mongo_connector, require_op_allowed,
    resolve_bind_params, timed_query, to_connect_error,
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
        })
    }

    pub(super) fn query(&self) -> &str {
        &self.query
    }

    pub(super) fn params(&self) -> &[Value] {
        &self.params
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

        let sqlx_query = bind_json_params(sqlx::query(read.query()), read.params());

        let max_rows = self.max_rows;
        let rows: Vec<AnyRow> = timed_query(db_config.query_timeout_ms, call.name, async {
            use futures::TryStreamExt;
            let mut stream = sqlx_query.fetch(&pool);
            let mut rows: Vec<AnyRow> = Vec::new();
            // Not `.map_err(|e| e.to_string())`: stringifying here converted
            // through `From<String>`, which is unconditionally a backend
            // failure, so a constraint the driver had already classified was
            // thrown away before `QueryFailure` could see it.
            while let Some(row) = stream.try_next().await? {
                if rows.len() >= max_rows {
                    // F42: classified so `timed_query` reports it as a 400 with
                    // the text intact rather than a 500 with the guidance
                    // sanitised away. The guidance *is* the message, so losing
                    // it loses the point.
                    return Err(
                        crate::engine::functions::connector_helpers::QueryFailure::Limit(format!(
                            "{} result exceeds query.max_limit ({max_rows} rows) — add a \
                             LIMIT to the query or raise the cap",
                            call.name
                        )),
                    );
                }
                rows.push(row);
            }
            Ok(rows)
        })
        .await?;

        Ok(Value::Array(rows_to_json(&rows)?).into())
    }
}

/// Convert AnyRow results to a JSON array of objects.
///
/// Column names are collected once from the first row and reused for all
/// subsequent rows, eliminating O(rows × columns) string allocations.
///
/// Only a genuine SQL `NULL` becomes `Value::Null`. A value the driver cannot
/// represent is an error, never a silent null — the two must stay
/// distinguishable to the workflow reading the result.
pub fn rows_to_json(rows: &[AnyRow]) -> Result<Vec<Value>, DataflowError> {
    if rows.is_empty() {
        return Ok(Vec::new());
    }

    let col_names: Vec<String> = rows[0]
        .columns()
        .iter()
        .map(|col| col.name().to_string())
        .collect();

    let mut result = Vec::with_capacity(rows.len());
    for row in rows {
        let mut obj = serde_json::Map::with_capacity(col_names.len());
        for (i, name) in col_names.iter().enumerate() {
            obj.insert(name.clone(), column_to_json(row, i, name)?);
        }
        result.push(Value::Object(obj));
    }
    Ok(result)
}

/// Decode one column of one row into JSON, dispatching on the value's own type
/// rather than probing candidate Rust types in turn.
///
/// The probe-cascade this replaced fell through to `Value::Null` for anything
/// it did not recognise, so `REAL` and `BLOB` columns — and any future
/// [`AnyTypeInfoKind`] — read back as null even though the query succeeded.
/// Matching the kind exhaustively means a new kind is a compile error instead.
fn column_to_json(row: &AnyRow, index: usize, name: &str) -> Result<Value, DataflowError> {
    let raw = row.try_get_raw(index).map_err(|e| {
        DataflowError::function_execution(
            format!("{NAME}: column '{name}' is unreadable: {e}"),
            None,
        )
    })?;
    if raw.is_null() {
        return Ok(Value::Null);
    }
    let kind = raw.type_info().kind();

    let decode_err = |e: sqlx::Error| {
        DataflowError::function_execution(
            format!("{NAME}: column '{name}' ({kind:?}) failed to decode: {e}"),
            None,
        )
    };

    let value = match kind {
        // Already handled above; a value whose own type info is NULL carries
        // nothing to decode.
        AnyTypeInfoKind::Null => Value::Null,
        AnyTypeInfoKind::Bool => Value::Bool(row.try_get::<bool, _>(index).map_err(decode_err)?),
        AnyTypeInfoKind::SmallInt | AnyTypeInfoKind::Integer | AnyTypeInfoKind::BigInt => {
            Value::Number(row.try_get::<i64, _>(index).map_err(decode_err)?.into())
        }
        AnyTypeInfoKind::Real => float_to_json(
            f64::from(row.try_get::<f32, _>(index).map_err(decode_err)?),
            name,
        )?,
        AnyTypeInfoKind::Double => {
            float_to_json(row.try_get::<f64, _>(index).map_err(decode_err)?, name)?
        }
        AnyTypeInfoKind::Text => {
            Value::String(row.try_get::<String, _>(index).map_err(decode_err)?)
        }
        AnyTypeInfoKind::Blob => {
            blob_to_json(row.try_get::<Vec<u8>, _>(index).map_err(decode_err)?)
        }
    };
    Ok(value)
}

/// JSON has no NaN or infinity. Rather than emit null — indistinguishable from
/// a SQL NULL — say so.
fn float_to_json(v: f64, name: &str) -> Result<Value, DataflowError> {
    serde_json::Number::from_f64(v)
        .map(Value::Number)
        .ok_or_else(|| {
            DataflowError::function_execution(
                format!("{NAME}: column '{name}' holds {v}, which JSON cannot represent"),
                None,
            )
        })
}

/// Binary columns become a string: the UTF-8 text when the bytes are valid
/// UTF-8 (MySQL reports `TEXT`/`JSON` columns as `BLOB`, so this is the common
/// case), otherwise lowercase hex.
fn blob_to_json(bytes: Vec<u8>) -> Value {
    match String::from_utf8(bytes) {
        Ok(s) => Value::String(s),
        Err(e) => Value::String(hex::encode(e.into_bytes())),
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
        name: "output",
        description: "Dotted path in the message where rows are written. Defaults to \"data\".",
        kind: FieldKind::String,
        template_at: &[""],
        ..FieldSchema::DEFAULT
    },
];
