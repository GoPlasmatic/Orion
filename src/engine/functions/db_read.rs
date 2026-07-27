use std::sync::Arc;

use async_trait::async_trait;
use dataflow_rs::engine::error::DataflowError;
use dataflow_rs::engine::functions::AsyncFunctionHandler;
use dataflow_rs::engine::task_context::TaskContext;
use dataflow_rs::engine::task_outcome::TaskOutcome;
use serde_json::Value;
use sqlx::any::{AnyRow, AnyTypeInfoKind};
use sqlx::{Column, Row, ValueRef};

use super::connector_helpers::{
    apply_output, bind_json_params, extract_output_path, profile_handler, require_db_connector,
    require_op_allowed, require_str_field, resolve_bind_params, resolve_connector, timed_query,
    to_exec_error,
};
use crate::connector::ConnectorRegistry;
use crate::connector::pool_cache::SqlPoolCache;

/// Executes SQL SELECT queries against external databases configured via connectors.
pub struct DbReadHandler {
    pub pool_cache: Arc<SqlPoolCache>,
    pub registry: Arc<ConnectorRegistry>,
}

#[async_trait]
impl AsyncFunctionHandler for DbReadHandler {
    type Input = Value;

    async fn execute(
        &self,
        ctx: &mut TaskContext<'_>,
        input: &Value,
    ) -> dataflow_rs::Result<TaskOutcome> {
        // Bind values are resolved against the message context first; the SQL
        // text itself is never message-derived, so parameters stay the only
        // request-controlled part of the statement.
        let params = resolve_bind_params(input, "db_read", ctx)?;

        profile_handler("db_read", input, async move {
            let connector_name = require_str_field(input, "connector", "db_read")?;
            let query = require_str_field(input, "query", "db_read")?;

            let connector_config = resolve_connector(&self.registry, connector_name).await?;
            let db_config = require_db_connector(&connector_config, connector_name)?;
            require_op_allowed(&db_config.operations, "read", connector_name)?;

            let pool = self
                .pool_cache
                .get_pool(connector_name, db_config)
                .await
                .map_err(to_exec_error)?;

            let sqlx_query = bind_json_params(sqlx::query(query), &params);

            let rows: Vec<AnyRow> = timed_query(
                db_config.query_timeout_ms,
                "db_read",
                sqlx_query.fetch_all(&pool),
            )
            .await?;

            let result = rows_to_json(&rows)?;

            let output_path = extract_output_path(input);
            apply_output(ctx, output_path, result);
            Ok(TaskOutcome::Success)
        })
        .await
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
pub fn rows_to_json(rows: &[AnyRow]) -> Result<Value, DataflowError> {
    if rows.is_empty() {
        return Ok(Value::Array(Vec::new()));
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
    Ok(Value::Array(result))
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
            format!("db_read: column '{name}' is unreadable: {e}"),
            None,
        )
    })?;
    if raw.is_null() {
        return Ok(Value::Null);
    }
    let kind = raw.type_info().kind();

    let decode_err = |e: sqlx::Error| {
        DataflowError::function_execution(
            format!("db_read: column '{name}' ({kind:?}) failed to decode: {e}"),
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
                format!("db_read: column '{name}' holds {v}, which JSON cannot represent"),
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
