use std::sync::Arc;

use async_trait::async_trait;
use dataflow_rs::engine::functions::AsyncFunctionHandler;
use dataflow_rs::engine::functions::config::FunctionConfig;
use dataflow_rs::engine::message::{Change, Message};
use datalogic_rs::DataLogic;
use serde_json::Value;
use sqlx::any::AnyRow;
use sqlx::{Column, Row};

use super::connector_helpers::{
    apply_output, bind_json_params, extract_custom_input, extract_output_path,
    require_db_connector, require_str_field, resolve_connector, timed_query, to_exec_error,
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
    async fn execute(
        &self,
        message: &mut Message,
        config: &FunctionConfig,
        _datalogic: Arc<DataLogic>,
    ) -> dataflow_rs::Result<(usize, Vec<Change>)> {
        let input = extract_custom_input(config, "db_read")?;
        let connector_name = require_str_field(input, "connector", "db_read")?;
        let query = require_str_field(input, "query", "db_read")?;
        let params = input.get("params").and_then(|v| v.as_array());

        let connector_config = resolve_connector(&self.registry, connector_name).await?;
        let db_config = require_db_connector(&connector_config, connector_name)?;

        let pool = self
            .pool_cache
            .get_pool(connector_name, db_config)
            .await
            .map_err(to_exec_error)?;

        let sqlx_query = sqlx::query(query);
        let sqlx_query = if let Some(params) = params {
            bind_json_params(sqlx_query, params)
        } else {
            sqlx_query
        };

        let rows: Vec<AnyRow> = timed_query(
            db_config.query_timeout_ms,
            "db_read",
            sqlx_query.fetch_all(&pool),
        )
        .await?;

        let result = rows_to_json(&rows);

        let output_path = extract_output_path(input);

        let changes = apply_output(message, output_path, result);
        Ok((1, changes))
    }
}

/// Convert AnyRow results to a JSON array of objects.
///
/// Column names are collected once from the first row and reused for all
/// subsequent rows, eliminating O(rows × columns) string allocations.
pub fn rows_to_json(rows: &[AnyRow]) -> Value {
    if rows.is_empty() {
        return Value::Array(Vec::new());
    }

    // Pre-collect column names once from the first row
    let col_names: Vec<String> = rows[0]
        .columns()
        .iter()
        .map(|col| col.name().to_string())
        .collect();

    let mut result = Vec::with_capacity(rows.len());
    for row in rows {
        let mut obj = serde_json::Map::with_capacity(col_names.len());
        for (i, name) in col_names.iter().enumerate() {
            // Try to extract as various types, falling back through the chain
            let val = if let Ok(v) = row.try_get::<String, _>(i) {
                Value::String(v)
            } else if let Ok(v) = row.try_get::<i64, _>(i) {
                Value::Number(v.into())
            } else if let Ok(v) = row.try_get::<f64, _>(i) {
                serde_json::Number::from_f64(v)
                    .map(Value::Number)
                    .unwrap_or(Value::Null)
            } else if let Ok(v) = row.try_get::<bool, _>(i) {
                Value::Bool(v)
            } else if let Ok(None::<String>) = row.try_get::<Option<String>, _>(i) {
                Value::Null
            } else {
                Value::Null
            };
            obj.insert(name.clone(), val);
        }
        result.push(Value::Object(obj));
    }
    Value::Array(result)
}
