use std::sync::Arc;

use async_trait::async_trait;
use dataflow_rs::engine::functions::AsyncFunctionHandler;
use dataflow_rs::engine::functions::config::FunctionConfig;
use dataflow_rs::engine::message::{Change, Message};
use datalogic_rs::DataLogic;
use serde_json::Value;

use super::connector_helpers::{
    apply_output, bind_json_params, extract_custom_input, extract_output_path,
    require_db_connector, require_str_field, resolve_connector, timed_query, to_exec_error,
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
            .map_err(to_exec_error)?;

        let sqlx_query = sqlx::query(query);
        let sqlx_query = if let Some(params) = params {
            bind_json_params(sqlx_query, params)
        } else {
            sqlx_query
        };

        let result = timed_query(
            db_config.query_timeout_ms,
            "db_write",
            sqlx_query.execute(&pool),
        )
        .await?;

        let output = serde_json::json!({
            "rows_affected": result.rows_affected(),
        });

        let output_path = extract_output_path(input);

        let changes = apply_output(message, output_path, output);
        Ok((1, changes))
    }
}
