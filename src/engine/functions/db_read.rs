use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use dataflow_rs::engine::error::DataflowError;
use dataflow_rs::engine::functions::AsyncFunctionHandler;
use dataflow_rs::engine::functions::config::FunctionConfig;
use dataflow_rs::engine::message::{Change, Message};
use datalogic_rs::DataLogic;
use serde_json::Value;
use sqlx::any::AnyRow;
use sqlx::{Column, Row};

use super::http_common::{get_nested, set_nested};
use crate::connector::pool_cache::SqlPoolCache;
use crate::connector::{ConnectorConfig, ConnectorRegistry};

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
        let input = match config {
            FunctionConfig::Custom { input, .. } => input,
            _ => {
                return Err(DataflowError::Validation(
                    "Expected Custom config for db_read".into(),
                ));
            }
        };

        let connector_name = input
            .get("connector")
            .and_then(|v| v.as_str())
            .ok_or_else(|| {
                DataflowError::Validation("db_read requires 'connector' field".into())
            })?;

        let query = input
            .get("query")
            .and_then(|v| v.as_str())
            .ok_or_else(|| DataflowError::Validation("db_read requires 'query' field".into()))?;

        let params = input.get("params").and_then(|v| v.as_array());

        let connector_config = self.registry.get(connector_name).await.ok_or_else(|| {
            DataflowError::function_execution(
                format!("Connector '{}' not found", connector_name),
                None,
            )
        })?;

        let db_config = match connector_config.as_ref() {
            ConnectorConfig::Db(c) => c,
            _ => {
                return Err(DataflowError::Validation(format!(
                    "Connector '{}' is not a database connector",
                    connector_name
                )));
            }
        };

        let pool = self
            .pool_cache
            .get_pool(connector_name, db_config)
            .await
            .map_err(|e| DataflowError::function_execution(e.to_string(), None))?;

        let mut sqlx_query = sqlx::query(query);
        if let Some(params) = params {
            for param in params {
                sqlx_query = match param {
                    Value::String(s) => sqlx_query.bind(s.clone()),
                    Value::Number(n) => {
                        if let Some(i) = n.as_i64() {
                            sqlx_query.bind(i)
                        } else if let Some(f) = n.as_f64() {
                            sqlx_query.bind(f)
                        } else {
                            sqlx_query.bind(n.to_string())
                        }
                    }
                    Value::Bool(b) => sqlx_query.bind(*b),
                    Value::Null => sqlx_query.bind(None::<String>),
                    _ => sqlx_query.bind(param.to_string()),
                };
            }
        }

        let timeout_ms = db_config.query_timeout_ms.unwrap_or(30_000);
        let rows: Vec<AnyRow> = tokio::time::timeout(
            Duration::from_millis(timeout_ms),
            sqlx_query.fetch_all(&pool),
        )
        .await
        .map_err(|_| {
            DataflowError::Timeout(format!("db_read query timed out after {}ms", timeout_ms))
        })?
        .map_err(|e| {
            DataflowError::function_execution(format!("db_read query failed: {}", e), None)
        })?;

        let result = rows_to_json(&rows);

        let output_path = input
            .get("output")
            .and_then(|v| v.as_str())
            .unwrap_or("data");

        let mut changes = Vec::new();
        let old_value = get_nested(&message.context, output_path);
        set_nested(&mut message.context, output_path, result.clone());
        message.invalidate_context_cache();

        changes.push(Change {
            path: Arc::from(output_path),
            old_value: Arc::new(old_value),
            new_value: Arc::new(result),
        });

        Ok((1, changes))
    }
}

/// Convert AnyRow results to a JSON array of objects.
pub fn rows_to_json(rows: &[AnyRow]) -> Value {
    let mut result = Vec::new();
    for row in rows {
        let mut obj = serde_json::Map::new();
        for (i, col) in row.columns().iter().enumerate() {
            let name = col.name().to_string();
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
            obj.insert(name, val);
        }
        result.push(Value::Object(obj));
    }
    Value::Array(result)
}
