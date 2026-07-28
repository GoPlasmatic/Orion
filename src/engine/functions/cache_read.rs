use std::sync::Arc;

use async_trait::async_trait;
use dataflow_rs::engine::functions::AsyncFunctionHandler;
use dataflow_rs::engine::task_context::TaskContext;
use dataflow_rs::engine::task_outcome::TaskOutcome;
use serde_json::Value;

use super::connector_helpers::{
    apply_output, extract_output_path, observed_handler, require_cache_connector,
    require_str_field, resolve_connector, resolve_required_str, to_exec_error,
};
use crate::connector::ConnectorRegistry;
use crate::connector::cache_backend::CachePool;

/// Workflow function handler for reading values from a cache backend.
pub struct CacheReadHandler {
    pub cache_pool: Arc<CachePool>,
    pub registry: Arc<ConnectorRegistry>,
}

#[async_trait]
impl AsyncFunctionHandler for CacheReadHandler {
    type Input = Value;

    async fn execute(
        &self,
        ctx: &mut TaskContext<'_>,
        input: &Value,
    ) -> dataflow_rs::Result<TaskOutcome> {
        // Resolve the key against the message context before the handler body
        // takes `ctx` mutably — `{"var": "data.id"}` is the whole point of a
        // per-request cache lookup.
        let key = resolve_required_str(input, "key", "cache_read", ctx)?;

        // F40: read the channel before the body borrows `ctx` mutably.
        let channel = super::extract_channel(ctx.message()).to_string();

        observed_handler("cache_read", input, &channel, async move {
            let connector_name = require_str_field(input, "connector", "cache_read")?;

            let connector_config = resolve_connector(&self.registry, connector_name).await?;
            let cache_config = require_cache_connector(&connector_config, connector_name)?;

            let backend = self
                .cache_pool
                .get_backend(connector_name, cache_config)
                .await
                .map_err(to_exec_error)?;

            let value = backend.get(&key).await.map_err(to_exec_error)?;

            // `cache_write` JSON-encodes everything, so parsing is its exact
            // inverse. The raw-string fallback is kept deliberately: a key
            // written by something other than Orion may hold a bare string,
            // and surfacing that as a string beats failing the task.
            let result = match value {
                Some(v) => serde_json::from_str::<Value>(&v).unwrap_or(Value::String(v)),
                None => Value::Null,
            };

            let output_path = extract_output_path(input);
            apply_output(ctx, output_path, result);
            Ok(TaskOutcome::Success)
        })
        .await
    }
}
