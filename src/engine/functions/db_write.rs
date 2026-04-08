use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use dataflow_rs::engine::error::DataflowError;
use dataflow_rs::engine::functions::AsyncFunctionHandler;
use dataflow_rs::engine::functions::config::FunctionConfig;
use dataflow_rs::engine::message::{Change, Message};
use datalogic_rs::DataLogic;
use serde_json::Value;

use super::connector_helpers::{
    apply_output, extract_custom_input, require_db_connector, require_str_field, resolve_connector,
};
use crate::connector::ConnectorRegistry;
use crate::connector::pool_cache::SqlPoolCache;

/// Executes SQL write queries (INSERT, UPDATE, DELETE) against external databases
/// configured via connectors.
pub struct DbWriteHandler {
    pub pool_cache: Arc<SqlPoolCache>,
    pub registry: Arc<ConnectorRegistry>,
}

#[async_trait]
impl AsyncFunctionHandler for DbWriteHandler {
    async fn execute(
        &self,
        message: &mut Message,
        config: &FunctionConfig,
        _datalogic: Arc<DataLogic>,
    ) -> dataflow_rs::Result<(usize, Vec<Change>)> {
        let input = extract_custom_input(config, "db_write")?;
        let connector_name = require_str_field(input, "connector", "db_write")?;
        let query = require_str_field(input, "query", "db_write")?;
        let params = input.get("params").and_then(|v| v.as_array());

        let connector_config = resolve_connector(&self.registry, connector_name).await?;
        let db_config = require_db_connector(&connector_config, connector_name)?;

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
        let result =
            tokio::time::timeout(Duration::from_millis(timeout_ms), sqlx_query.execute(&pool))
                .await
                .map_err(|_| {
                    DataflowError::Timeout(format!(
                        "db_write query timed out after {}ms",
                        timeout_ms
                    ))
                })?
                .map_err(|e| {
                    DataflowError::function_execution(format!("db_write query failed: {}", e), None)
                })?;

        let output = serde_json::json!({
            "rows_affected": result.rows_affected(),
        });

        let output_path = input
            .get("output")
            .and_then(|v| v.as_str())
            .unwrap_or("data");

        let changes = apply_output(message, output_path, output);
        Ok((1, changes))
    }
}
