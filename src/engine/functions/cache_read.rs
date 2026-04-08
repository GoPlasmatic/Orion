use std::sync::Arc;

use async_trait::async_trait;
use dataflow_rs::engine::functions::AsyncFunctionHandler;
use dataflow_rs::engine::functions::config::FunctionConfig;
use dataflow_rs::engine::message::{Change, Message};
use datalogic_rs::DataLogic;
use serde_json::Value;

use super::connector_helpers::{
    apply_output, extract_custom_input, extract_output_path, require_cache_connector,
    require_str_field, resolve_connector, to_exec_error,
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
    async fn execute(
        &self,
        message: &mut Message,
        config: &FunctionConfig,
        _datalogic: Arc<DataLogic>,
    ) -> dataflow_rs::Result<(usize, Vec<Change>)> {
        let input = extract_custom_input(config, "cache_read")?;
        let connector_name = require_str_field(input, "connector", "cache_read")?;
        let key = require_str_field(input, "key", "cache_read")?;

        let connector_config = resolve_connector(&self.registry, connector_name).await?;
        let cache_config = require_cache_connector(&connector_config, connector_name)?;

        let backend = self
            .cache_pool
            .get_backend(connector_name, cache_config)
            .await
            .map_err(to_exec_error)?;

        let value = backend.get(key).await.map_err(to_exec_error)?;

        let result = match value {
            Some(v) => {
                // Try to parse as JSON, fall back to string
                serde_json::from_str::<Value>(&v).unwrap_or(Value::String(v))
            }
            None => Value::Null,
        };

        let output_path = extract_output_path(input);

        let changes = apply_output(message, output_path, result);
        Ok((1, changes))
    }
}
