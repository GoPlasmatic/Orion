use std::sync::Arc;

use async_trait::async_trait;
use dataflow_rs::engine::error::DataflowError;
use dataflow_rs::engine::functions::AsyncFunctionHandler;
use dataflow_rs::engine::functions::config::FunctionConfig;
use dataflow_rs::engine::message::{Change, Message};
use datalogic_rs::DataLogic;
use serde_json::Value;

use super::connector_helpers::{
    extract_custom_input, require_cache_connector, require_str_field, resolve_connector,
    to_exec_error,
};
use crate::connector::ConnectorRegistry;
use crate::connector::cache_backend::CachePool;

/// Workflow function handler for writing values to a cache backend.
pub struct CacheWriteHandler {
    pub cache_pool: Arc<CachePool>,
    pub registry: Arc<ConnectorRegistry>,
}

#[async_trait]
impl AsyncFunctionHandler for CacheWriteHandler {
    async fn execute(
        &self,
        _message: &mut Message,
        config: &FunctionConfig,
        _datalogic: Arc<DataLogic>,
    ) -> dataflow_rs::Result<(usize, Vec<Change>)> {
        let input = extract_custom_input(config, "cache_write")?;
        let connector_name = require_str_field(input, "connector", "cache_write")?;
        let key = require_str_field(input, "key", "cache_write")?;

        let connector_config = resolve_connector(&self.registry, connector_name).await?;
        let cache_config = require_cache_connector(&connector_config, connector_name)?;

        let backend = self
            .cache_pool
            .get_backend(connector_name, cache_config)
            .await
            .map_err(to_exec_error)?;

        // Serialize the value to a string for storage
        let value_str = match input.get("value") {
            Some(Value::String(s)) => s.clone(),
            Some(v) => serde_json::to_string(v).map_err(|e| {
                DataflowError::Validation(format!("Failed to serialize value for cache: {}", e))
            })?,
            None => {
                return Err(DataflowError::Validation(
                    "cache_write requires 'value'".into(),
                ));
            }
        };

        // Optional TTL in seconds
        let ttl = input.get("ttl_secs").and_then(|v| v.as_u64());

        if let Some(ttl) = ttl {
            backend
                .set_ex(key, &value_str, ttl)
                .await
                .map_err(to_exec_error)?;
        } else {
            backend.set(key, &value_str).await.map_err(to_exec_error)?;
        }

        tracing::debug!(
            key = %key,
            ttl = ?ttl,
            "Wrote value to cache"
        );

        Ok((1, vec![]))
    }
}
