use std::sync::Arc;

use async_trait::async_trait;
use dataflow_rs::engine::functions::AsyncFunctionHandler;
use dataflow_rs::engine::task_context::TaskContext;
use dataflow_rs::engine::task_outcome::TaskOutcome;
use serde_json::Value;

use super::connector_helpers::{
    apply_output, bind_json_params, extract_output_path, profile_handler, require_db_connector,
    require_op_allowed, require_str_field, resolve_bind_params, resolve_connector, timed_query,
    to_exec_error,
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
    type Input = Value;

    async fn execute(
        &self,
        ctx: &mut TaskContext<'_>,
        input: &Value,
    ) -> dataflow_rs::Result<TaskOutcome> {
        // Bind values are resolved against the message context first; the SQL
        // text itself is never message-derived, so parameters stay the only
        // request-controlled part of the statement.
        let params = resolve_bind_params(input, "db_write", ctx)?;

        profile_handler("db_write", input, async move {
            let connector_name = require_str_field(input, "connector", "db_write")?;
            let query = require_str_field(input, "query", "db_write")?;

            let connector_config = resolve_connector(&self.registry, connector_name).await?;
            let db_config = require_db_connector(&connector_config, connector_name)?;
            // Raw SQL cannot be classified per-op; it has its own gate.
            require_op_allowed(&db_config.operations, "raw_write", connector_name)?;

            let pool = self
                .pool_cache
                .get_pool(connector_name, db_config)
                .await
                .map_err(to_exec_error)?;

            let sqlx_query = bind_json_params(sqlx::query(query), &params);

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
            apply_output(ctx, output_path, output);
            Ok(TaskOutcome::Success)
        })
        .await
    }
}
